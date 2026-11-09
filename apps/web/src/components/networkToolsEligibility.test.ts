import { expect, it } from "vitest";
import cases from "../../../../crates/astra-client/src/test/network_eligibility_cases.json";
import type { DiagnosticRequest } from "./networkToolsModel";
import {
  needsResolutionCapability,
  validateLegacyRequest,
} from "./networkToolsEligibility";

it.each(cases)(
  "native/Web admission parity: $name",
  ({ request, legacy, resolution }) => {
    const typed = request as DiagnosticRequest;
    expect(needsResolutionCapability(typed)).toBe(resolution);
    if (legacy) expect(() => validateLegacyRequest(typed)).not.toThrow();
    else expect(() => validateLegacyRequest(typed)).toThrow();
  },
);
