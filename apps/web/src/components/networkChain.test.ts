import { expect, it } from "vitest";
import type { Egress } from "../generated/management";
import cases from "../../../../crates/astra-client/src/test/fixtures/network_chain_cases.json";
import { chainText, liveChain, pathStrips, stripRowText } from "./networkChain";

// Shared with `nettools_test.rs::live_chain_matches_shared_cases`.
it.each(cases)("$name", (c) => {
  expect(chainText(liveChain(c.runtime, c.network, c.egress as Egress))).toBe(
    c.expected,
  );
});

it("linearises backend topology into rows per section", () => {
  const strips = pathStrips({
    topology: [
      {
        path_id: "path-a",
        truncated: false,
        nodes: [
          { id: "agent", label: "目标 Agent", role: "agent" },
          { id: "interface:en0", label: "en0", role: "interface" },
          { id: "group:PROXY:hk", label: "PROXY → hk", role: "proxy_group" },
          { id: "target", label: "example.com", role: "target" },
          { id: "dns", label: "1.1.1.1", role: "dns" },
        ],
        edges: [
          {
            from: "interface:en0",
            to: "group:PROXY:hk",
            section: "connection",
            level: "configuration",
          },
          {
            from: "agent",
            to: "interface:en0",
            section: "connection",
            level: "configuration",
          },
          {
            from: "group:PROXY:hk",
            to: "target",
            section: "connection",
            level: "measured",
          },
          { from: "agent", to: "dns", section: "dns", level: "measured" },
        ],
        attachments: [{ kind: "nat", label: "端口受限锥型" }],
      },
    ],
  });
  expect(strips[0].rows.map(stripRowText)).toEqual([
    "[连接] 目标 Agent ┄› en0 ┄› PROXY → hk ─› example.com",
    "[DNS] 目标 Agent ─› 1.1.1.1",
  ]);
  expect(strips[0].rows[0].steps[3].edge).toBe(2);
  expect(strips[0].attachments).toEqual(["nat：端口受限锥型"]);
});
