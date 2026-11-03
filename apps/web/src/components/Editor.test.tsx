// @vitest-environment jsdom
import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import Editor from "./Editor";
import * as API from "../api";
import { recover, retain } from "../recovery";
vi.mock("../api", async (original) => ({
  ...(await original<typeof API>()),
  draftCommand: vi.fn(),
}));
vi.mock("../recovery", () => ({ recover: vi.fn(), retain: vi.fn() }));
const instance: API.Instance = {
  id: "instance-a",
  name: "proxy",
  kind: "proxy",
  config: "proxy.toml",
  pid: null,
  state: "stopped",
  uptime_secs: "0",
  capabilities: ["document"],
  changed: false,
  details: null,
};
const initial: API.Draft = {
  id: "draft-a",
  instance: instance.id,
  path: "proxy.toml",
  original: "# original\n",
  text: "# original\n",
  revision: "1",
  source_revision: "source-a",
  kind: "proxy",
};
const response = (data: API.Draft) => ({ type: "draft", data });
const deferred = <T,>() => {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
};
beforeEach(() => {
  vi.resetAllMocks();
  vi.mocked(recover).mockResolvedValue(undefined);
  vi.mocked(retain).mockResolvedValue();
});
afterEach(cleanup);
describe("document concurrency and recovery", () => {
  it("preserves edits made while an earlier save is pending and advances only its source baseline", async () => {
    const pending = deferred<ReturnType<typeof response>>();
    vi.mocked(API.draftCommand).mockImplementation(
      async (_agent, command, data) => {
        if (command === "open") return response(initial);
        if (command === "edit")
          return response({
            ...initial,
            text: (data as { text: string }).text,
            revision: "2",
          });
        if (command === "save") return pending.promise;
        throw new Error(command);
      },
    );
    const onSaved = vi.fn();
    render(
      <Editor
        agent="agent-a"
        instance={instance}
        onClose={vi.fn()}
        onSaved={onSaved}
        onError={vi.fn()}
      />,
    );
    const input = await screen.findByRole("textbox", { name: "TOML 配置文档" });
    fireEvent.change(input, { target: { value: "# submitted\n" } });
    fireEvent.click(screen.getByRole("button", { name: "保存并应用" }));
    await waitFor(() =>
      expect(API.draftCommand).toHaveBeenCalledWith(
        "agent-a",
        "save",
        expect.objectContaining({
          expected_source_revision: "source-a",
          expected_revision: "2",
        }),
      ),
    );
    fireEvent.change(input, { target: { value: "# later edit\n" } });
    await act(async () =>
      pending.resolve(
        response({
          ...initial,
          original: "# submitted\n",
          text: "# submitted\n",
          revision: "3",
          source_revision: "source-b",
        }),
      ),
    );
    expect((input as HTMLTextAreaElement).value).toBe("# later edit\n");
    await waitFor(() =>
      expect(retain).toHaveBeenLastCalledWith(
        expect.stringContaining("agent-a/instance-a"),
        expect.objectContaining({
          text: "# later edit\n",
          original: "# submitted\n",
          sourceRevision: "source-b",
          revision: "3",
        }),
      ),
    );
    expect(onSaved).toHaveBeenCalledOnce();
  });
  it("reopens the persisted draft so recovered input keeps its original conflict baseline", async () => {
    vi.mocked(recover).mockResolvedValue({
      draftId: initial.id,
      text: "# offline edit\n",
      revision: "1",
      original: initial.original,
      sourceRevision: "source-a",
    });
    vi.mocked(API.draftCommand).mockResolvedValue(response(initial));
    render(
      <Editor
        agent="agent-a"
        instance={instance}
        onClose={vi.fn()}
        onSaved={vi.fn()}
        onError={vi.fn()}
      />,
    );
    fireEvent.click(await screen.findByRole("button", { name: "恢复内容" }));
    expect((screen.getByRole("textbox") as HTMLTextAreaElement).value).toBe(
      "# offline edit\n",
    );
    expect(API.draftCommand).toHaveBeenCalledWith("agent-a", "read", {
      draft_id: "draft-a",
    });
    expect(API.draftCommand).not.toHaveBeenCalledWith(
      expect.anything(),
      "open",
      expect.anything(),
    );
    expect(retain).toHaveBeenLastCalledWith(
      expect.anything(),
      expect.objectContaining({ sourceRevision: "source-a" }),
    );
  });
  it("does not let a late result from the previous node replace the selected document", async () => {
    const old = deferred<ReturnType<typeof response>>();
    vi.mocked(API.draftCommand).mockImplementation(async (agent) =>
      agent === "agent-a"
        ? old.promise
        : response({
            ...initial,
            id: "draft-b",
            instance: "instance-b",
            text: "# node b\n",
          }),
    );
    const props = { onClose: vi.fn(), onSaved: vi.fn(), onError: vi.fn() };
    const view = render(
      <Editor agent="agent-a" instance={instance} {...props} />,
    );
    await waitFor(() => expect(API.draftCommand).toHaveBeenCalledOnce());
    view.rerender(
      <Editor
        agent="agent-b"
        instance={{ ...instance, id: "instance-b" }}
        {...props}
      />,
    );
    const input = await screen.findByRole("textbox");
    await act(async () => old.resolve(response(initial)));
    expect((input as HTMLTextAreaElement).value).toBe("# node b\n");
    expect(props.onError).not.toHaveBeenCalled();
  });
});
