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
