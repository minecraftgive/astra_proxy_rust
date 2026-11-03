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
  query: vi.fn(),
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
  it("journals the original save ID and queries it instead of resubmitting an uncertain save", async () => {
    let operationId = "";
    vi.mocked(API.draftCommand).mockImplementation(
      async (_agent, command, data) => {
        if (command === "save") {
          operationId = (data as { operation_id: string }).operation_id;
          expect(retain).toHaveBeenCalledWith(
            expect.anything(),
            expect.objectContaining({ saveOperationId: operationId }),
          );
          throw new API.ApiError(
            "outcome_unknown",
            "lost response",
            undefined,
            operationId,
          );
        }
        return response(initial);
      },
    );
    vi.mocked(API.query).mockImplementation(async () => ({
      type: "task",
      data: {
        id: operationId,
        state: "complete",
        message: "Saved",
        started_ms: "0",
        finished_ms: "1",
      },
    }));
    render(
      <Editor
        agent="agent-a"
        instance={instance}
        onClose={vi.fn()}
        onSaved={vi.fn()}
        onError={vi.fn()}
      />,
    );
    await screen.findByRole("textbox");
    fireEvent.click(screen.getByRole("button", { name: "保存并应用" }));
    const inspect = await screen.findByRole("button", {
      name: "查询原保存任务",
    });
    await waitFor(() =>
      expect((inspect as HTMLButtonElement).disabled).toBe(false),
    );
    expect(
      (screen.getByRole("button", { name: "保存并应用" }) as HTMLButtonElement)
        .disabled,
    ).toBe(true);
    fireEvent.click(inspect);
    await waitFor(() =>
      expect(API.query).toHaveBeenCalledWith("agent-a", "task", {
        operation_id: operationId,
      }),
    );
    await waitFor(() =>
      expect(
        screen.queryByRole("button", { name: "查询原保存任务" }),
      ).toBeNull(),
    );
    expect(
      vi
        .mocked(API.draftCommand)
        .mock.calls.filter(([, command]) => command === "save"),
    ).toHaveLength(1);
  });
  it("does not send a save when its recovery identity cannot be persisted", async () => {
    vi.mocked(API.draftCommand).mockResolvedValue(response(initial));
    vi.mocked(retain).mockRejectedValue(new Error("disk full"));
    render(
      <Editor
        agent="agent-a"
        instance={instance}
        onClose={vi.fn()}
        onSaved={vi.fn()}
        onError={vi.fn()}
      />,
    );
    await screen.findByRole("textbox");
    fireEvent.click(screen.getByRole("button", { name: "保存并应用" }));
    await waitFor(() =>
      expect(API.draftCommand).toHaveBeenCalledWith(
        "agent-a",
        "edit",
        expect.anything(),
      ),
    );
    expect(
      vi
        .mocked(API.draftCommand)
        .mock.calls.some(([, command]) => command === "save"),
    ).toBe(false);
  });
  it("reopens an uncertain save with its original identity and keeps the unsynchronized recovery copy", async () => {
    vi.mocked(recover).mockResolvedValue({
      draftId: initial.id,
      text: "# local edits",
      revision: "1",
      saveOperationId: "pending-save",
    });
    vi.mocked(API.draftCommand).mockResolvedValue(response(initial));
    vi.mocked(API.query).mockResolvedValue({
      type: "task",
      data: {
        id: "pending-save",
        state: "outcome_unknown",
        message: "Inspect runtime",
        started_ms: "0",
        finished_ms: null,
      },
    });
    render(
      <Editor
        agent="agent-a"
        instance={instance}
        onClose={vi.fn()}
        onSaved={vi.fn()}
        onError={vi.fn()}
      />,
    );
    fireEvent.click(
      await screen.findByRole("button", { name: "查询原保存任务" }),
    );
    await waitFor(() =>
      expect(retain).toHaveBeenLastCalledWith(
        expect.anything(),
        expect.objectContaining({
          text: "# local edits",
          saveOperationId: "pending-save",
        }),
      ),
    );
    expect(
      (screen.getByRole("button", { name: "保存并应用" }) as HTMLButtonElement)
        .disabled,
    ).toBe(true);
    expect(
      vi
        .mocked(API.draftCommand)
        .mock.calls.some(([, command]) => command === "save"),
    ).toBe(false);
  });
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
