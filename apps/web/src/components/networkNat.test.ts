import { expect, it } from "vitest";
import cases from "../../../../crates/astra-client/src/test/fixtures/network_nat_cases.json";
import { NAT_TABLE, natType } from "./networkNat";

// Shared with `nettools_test.rs::nat_type_matches_shared_cases`.
it.each(cases)("$name", (c) => {
  expect(natType(c.report).kind).toBe(c.expected);
});

it("the comparison table lists the four traditional categories", () => {
  expect(NAT_TABLE.map((row) => [row.kind, row.p2p])).toEqual([
    ["full_cone", "极好"],
    ["restricted_cone", "良好"],
    ["port_restricted_cone", "一般"],
    ["symmetric", "差"],
  ]);
});
