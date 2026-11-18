// @vitest-environment jsdom
import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import SchemaEditor from "./SchemaEditor";
import { request, type Draft } from "../api";
vi.mock("../api", () => ({ request: vi.fn() }));
afterEach(cleanup);
it("reorders relay members in the draft without silently changing the saved path", async () => {
  vi.mocked(request).mockResolvedValue({
    data: {
      core: "proxy",
      path: [],
      collection: false,
      rows: [
        {
          kind: "field",
          field: {
            key: "members",
            label: "Group members",
            field_type: { kind: "string_list" },
            value: '["entry","middle","exit"]',
            default: "[]",
            required: false,
            secret: false,
            help: "entry → relay → exit",
          },
        },
      ],
    },
  } as never);
  const onCommand = vi.fn().mockResolvedValue(undefined);
  const draft = { id: "chain", revision: "1" } as Draft;
  render(
    <SchemaEditor
      agent="local"
      draft={draft}
      busy={false}
      onCommand={onCommand}
      onError={vi.fn()}
    />,
  );
  fireEvent.click(await screen.findByRole("button", { name: "上移 exit" }));
  expect(onCommand).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole("button", { name: "写入草稿" }));
  await waitFor(() =>
    expect(onCommand).toHaveBeenCalledWith({
      command: "set_field",
      path: [],
      key: "members",
      input: '["entry","exit","middle"]',
    }),
  );
});

it("displays entry summaries from draft text and moves entries down", async () => {
  vi.mocked(request)
    .mockResolvedValueOnce({
      data: {
        core: "proxy",
        path: [],
        collection: false,
        rows: [{ kind: "group", key: "outbounds", label: "Outbounds" }],
      },
    } as never)
    .mockResolvedValueOnce({
      data: {
        core: "proxy",
        path: [{ Key: "outbounds" }],
        collection: true,
        rows: [
          { kind: "entry", index: 0 },
          { kind: "entry", index: 1 },
        ],
      },
    } as never);
  const onCommand = vi.fn().mockResolvedValue(undefined);
  const draft = {
    id: "nodes",
    revision: "2",
    text: [
      "[[outbounds]]",
      'tag = "hk-01"',
      'kind = "native"',
      "[outbounds.config]",
      'protocol = "trojan"',
      "[[outbounds]]",
      'tag = "us-02"',
      'kind = "socks5"',
    ].join("\n"),
  } as Draft;
  render(
    <SchemaEditor
      agent="local"
      draft={draft}
      busy={false}
      onCommand={onCommand}
      onError={vi.fn()}
    />,
  );
  fireEvent.click(await screen.findByRole("button", { name: "Outbounds →" }));
  expect(
    await screen.findByRole("button", { name: "条目 1 · hk-01 [trojan] →" }),
  ).toBeTruthy();
  expect(
    screen.getByRole("button", { name: "条目 2 · us-02 [socks5] →" }),
  ).toBeTruthy();
  const downButtons = screen.getAllByRole("button", { name: "下移" });
  expect((downButtons[1] as HTMLButtonElement).disabled).toBe(true);
  fireEvent.click(downButtons[0]);
  await waitFor(() =>
    expect(onCommand).toHaveBeenCalledWith({
      command: "move_entry",
      path: [{ Key: "outbounds" }],
      from: 0,
      to: 1,
    }),
  );
});

it("toggles secret field visibility between password and plain text", async () => {
  vi.mocked(request).mockResolvedValue({
    data: {
      core: "proxy",
      path: [],
      collection: false,
      rows: [
        {
          kind: "field",
          field: {
            key: "password",
            label: "Password",
            field_type: { kind: "string" },
            value: "secret-pass",
            default: "",
            required: true,
            secret: true,
            help: "Secret token",
          },
        },
      ],
    },
  } as never);
  render(
    <SchemaEditor
      agent="local"
      draft={{ id: "s", revision: "1", text: "" } as Draft}
      busy={false}
      onCommand={vi.fn()}
      onError={vi.fn()}
    />,
  );
  const input = (await screen.findByLabelText(/Password/)) as HTMLInputElement;
  expect(input.type).toBe("password");
  fireEvent.click(screen.getByRole("button", { name: "显示" }));
  expect(input.type).toBe("text");
  fireEvent.click(screen.getByRole("button", { name: "隐藏" }));
  expect(input.type).toBe("password");
});
