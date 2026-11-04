"""Terminal build form and actual target-width validation without compiling Rust."""
import contextlib
import importlib.util
import io
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
spec = importlib.util.spec_from_file_location('astra_build_form_cli', SCRIPTS / 'build.py')
build = importlib.util.module_from_spec(spec)
spec.loader.exec_module(build)


class BuildFormTests(unittest.TestCase):
    def run_form(self, args, answers, *, terminal=True):
        output = io.StringIO()
        with patch.object(sys.stdin, 'isatty', return_value=terminal), \
             patch.object(sys.stdout, 'isatty', return_value=terminal), \
             patch('builtins.input', side_effect=answers) as prompt, \
             contextlib.redirect_stdout(output):
            # redirect_stdout replaces the stream whose TTY status the form reads.
            with patch.object(output, 'isatty', return_value=terminal):
                build.interactive_options(args)
        return output.getvalue(), prompt

    def test_edits_profile_architecture_and_optional_build_fields(self):
        args = build.argument_parser().parse_args(['--profile', 'release', '--role', 'controller'])
        output, _ = self.run_form(args, [
            '1', '3', '2', '64bits', '3', 'web', '4', 'yes', '5', 'yes', 'b',
        ])
        self.assertEqual((args.profile, args.arch, args.role), ('both', '64bits', 'web'))
        self.assertTrue(args.package)
        self.assertTrue(args.timings)
        self.assertIn('Cargo profiles: release', output)
        self.assertIn('Cargo profiles: dev, release', output)

    def test_invalid_input_retries_and_blank_keeps_prefilled_values(self):
        args = build.argument_parser().parse_args(['--profile', 'release', '--arch', '64bits'])
        output, prompt = self.run_form(args, ['', 'unknown', '1', '99', '', 'build'])
        self.assertEqual((args.profile, args.arch), ('release', '64bits'))
        self.assertEqual(prompt.call_count, 6)
        self.assertIn('Enter a field number', output)
        self.assertIn('Invalid choice', output)

    def test_mode_option_was_removed(self):
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as error:
            build.argument_parser().parse_args(['--mode', 'runtime'])
        self.assertEqual(error.exception.code, 2)

    def test_cancel_at_top_level_or_field_does_not_start_build(self):
        for answers, exit_code in [(['q'], 0), (['1', 'cancel'], 0),
                                   ([EOFError()], 0), ([KeyboardInterrupt()], 130)]:
            with self.subTest(answers=answers):
                args = build.argument_parser().parse_args([])
                with self.assertRaises(SystemExit) as error:
                    self.run_form(args, answers)
                self.assertEqual(error.exception.code, exit_code)

    def test_non_terminal_skips_form_unless_explicitly_requested(self):
        args = build.argument_parser().parse_args([])
        output, prompt = self.run_form(args, [], terminal=False)
        prompt.assert_not_called()
        self.assertEqual(output, '')
        args = build.argument_parser().parse_args(['--interactive'])
        output, prompt = self.run_form(args, ['b'], terminal=False)
        prompt.assert_called_once()
        self.assertIn('Astra build form', output)

    def test_interaction_switches_are_mutually_exclusive(self):
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as error:
            build.argument_parser().parse_args(['--interactive', '--non-interactive'])
        self.assertEqual(error.exception.code, 2)


class ArchitectureSelectionTests(unittest.TestCase):
    def test_64bits_checks_actual_rust_target_for_both_cpu_families(self):
        environment = {'PATH': 'test-toolchain'}
        for target in ('x86_64-pc-windows-msvc', 'aarch64-apple-darwin'):
            with self.subTest(target=target), \
                 patch.object(build.subprocess, 'check_output',
                              return_value='target_os="test"\ntarget_pointer_width="64"\n') as rustc:
                build.validate_architecture('64bits', target, environment)
                rustc.assert_called_once_with(
                    ['rustc', '--print', 'cfg', '--target', target],
                    cwd=build.ROOT, env=environment, text=True)

    def test_64bits_rejects_a_32bit_rust_target(self):
        with patch.object(build.subprocess, 'check_output',
                          return_value='target_pointer_width="32"\n'), \
             self.assertRaisesRegex(ValueError, '64-bit Rust host target'):
            build.validate_architecture('64bits', 'i686-pc-windows-msvc', {})

    def test_native_retains_current_target_without_width_probe(self):
        with patch.object(build.subprocess, 'check_output') as rustc:
            build.validate_architecture('native', 'i686-pc-windows-msvc', {})
        rustc.assert_not_called()


if __name__ == '__main__':
    unittest.main()
