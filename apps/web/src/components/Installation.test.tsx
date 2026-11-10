// @vitest-environment jsdom
import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import * as API from "../api";
import type {
  InstallationStatus,
  InstallationResult,
} from "../generated/management";
import Installation from "./Installation";

vi.mock("../api", async (original) => ({
  ...(await original<typeof API>()),
  query: vi.fn(),
  request: vi.fn(),
  signedIn: () => true,
}));
const sample = (
  patch: Partial<InstallationStatus> = {},
): InstallationStatus => ({
  target: "target-machine",
  account: "target-user",
  packages: [
    {
      id: "current-client",
      role: "client",
      release_id: "release-1",
      source: "/unpacked/client",
      user: {
        directory: "/home/user/.local/share/astra/client",
        entry: "/home/user/.local/bin/astra-client",
        installed: false,
        requires_elevation: false,
        unavailable_reason: null,
      },
      system: {
        directory: "/opt/astra/client",
        entry: "/usr/local/bin/astra-client",
        installed: false,
        requires_elevation: true,
        unavailable_reason: null,
      },
    },
  ],
  unavailable_roles: {
    server: "缺少服务端核心",
    controller: "没有控制器包",
    web: "缺少网页资源",
  },
  warnings: [],
  ...patch,
});
const installationResult: InstallationResult = {
  role: "client",
  directory: "/home/user/.local/share/astra/client",
  entry: "/home/user/.local/bin/astra-client",
  already_installed: false,
  message: "完整程序包已安装",
  launch_command: "/home/user/.local/bin/astra-client",
};
const completed = (id = "original-operation"): API.Task => ({
  id,
  state: "complete",
  message: "安装任务已完成",
  started_ms: "100",
  finished_ms: "200",
});
let sequence = 0;
let props: Parameters<typeof Installation>[0];
beforeEach(() => {
  vi.mocked(API.query).mockReset();
  vi.mocked(API.request).mockReset();
  const storage = new Map<string, string>();
  vi.stubGlobal("localStorage", {
    getItem: (key: string) => storage.get(key) ?? null,
    setItem: (key: string, value: string) => storage.set(key, value),
    removeItem: (key: string) => storage.delete(key),
  });
  props = {
    agent: `target-${++sequence}`,
    nodeName: "测试节点",
    online: true,
    active: true,
    onOpen: vi.fn(),
    onError: vi.fn(),
  };
  vi.mocked(API.query).mockImplementation(async (_agent, type, data) => {
    if (type === "hello")
      return { type: "hello", data: { capabilities: ["installation"] } };
    if (type === "installation_status") return { type, data: sample() };
    if (type === "task")
      return {
        type,
        data: completed((data as { operation_id: string }).operation_id),
      };
    throw new Error(`Unexpected query: ${type}`);
  });
  vi.mocked(API.request).mockResolvedValue({
    type: "installation_result",
    data: installationResult,
  });
});
afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});
const ready = () => screen.findByRole("button", { name: "安装当前完整包" });

it("shows one reminder per target, retains skip and keeps the settings entry usable", async () => {
  const view = render(<Installation {...props} active={false} />);
  fireEvent.click(await screen.findByRole("button", { name: "暂不安装" }));
  expect(screen.queryByLabelText("安装提醒")).toBeNull();
  view.unmount();
  const next = render(<Installation {...props} active={false} />);
  await waitFor(() => expect(API.query).toHaveBeenCalledTimes(4));
  expect(screen.queryByLabelText("安装提醒")).toBeNull();
  next.rerender(<Installation {...props} active />);
  await ready();
  expect(screen.getByLabelText("仅目标账户")).toHaveProperty("checked", true);
  expect(screen.getByText("缺少服务端核心")).toBeTruthy();
  expect(screen.getByText("没有控制器包")).toBeTruthy();
  expect(screen.getByText("缺少网页资源")).toBeTruthy();
  next.rerender(<Installation {...props} agent="new-target" active={false} />);
  expect(await screen.findByRole("button", { name: "前往安装" })).toBeTruthy();
});

it("opens settings from the reminder without starting an installation", async () => {
  render(<Installation {...props} active={false} />);
  fireEvent.click(await screen.findByRole("button", { name: "前往安装" }));
  expect(props.onOpen).toHaveBeenCalledOnce();
  expect(API.request).not.toHaveBeenCalled();
});

it.each(["missing", "installed"])(
  "shows the first supported-target reminder when packages are %s",
  async (state) => {
    const status = sample();
    if (state === "missing") status.packages = [];
    else status.packages[0].user.installed = true;
    vi.mocked(API.query).mockImplementation(async (_agent, type) => ({
      type,
      data: type === "hello" ? { capabilities: ["installation"] } : status,
    }));
    const first = render(<Installation {...props} active={false} />);
    expect(
      await screen.findByRole("button", { name: "前往安装" }),
    ).toBeTruthy();
    first.unmount();
    render(<Installation {...props} active={false} />);
    await waitFor(() => expect(API.query).toHaveBeenCalledTimes(4));
    expect(screen.queryByLabelText("安装提醒")).toBeNull();
  },
);

it("checks old-node capabilities before sending a new installation query", async () => {
  vi.mocked(API.query).mockResolvedValue({
    type: "hello",
    data: { capabilities: ["instances"] },
  });
  render(<Installation {...props} />);
  expect(await screen.findByText(/此节点版本未提供安装功能/)).toBeTruthy();
  expect(API.query).toHaveBeenCalledTimes(1);
  expect(API.query).toHaveBeenCalledWith(
    props.agent,
    "hello",
    undefined,
    expect.any(AbortSignal),
  );
  expect(screen.queryByRole("button", { name: "安装当前完整包" })).toBeNull();
});

it("uses user scope by default, sends the release precondition and displays the result", async () => {
  render(<Installation {...props} />);
  fireEvent.click(await ready());
  expect(await screen.findByText(/安装完成。/)).toBeTruthy();
  expect(API.request).toHaveBeenCalledOnce();
  expect(API.request).toHaveBeenCalledWith(props.agent, {
    type: "execute",
    data: {
      operation_id: expect.any(String),
      command: {
        type: "install_current_package",
        data: {
          package_id: "current-client",
          expected_release_id: "release-1",
          scope: "user",
        },
      },
    },
  });
  await waitFor(() =>
    expect(
      vi
        .mocked(API.query)
        .mock.calls.filter((call) => call[1] === "installation_status"),
    ).toHaveLength(2),
  );
});

it("shows target-machine authorization instructions for system installs without invoking a local helper", async () => {
  render(<Installation {...props} />);
  await ready();
  fireEvent.click(screen.getByLabelText("目标机器的所有用户"));
  expect(screen.getByText("/opt/astra/client")).toBeTruthy();
  expect(screen.getByRole("alert").textContent).toContain("target-machine");
  expect(screen.getByRole("alert").textContent).toContain(
    "网页不会请求当前电脑",
  );
  expect(screen.getByRole("button", { name: "安装当前完整包" })).toHaveProperty(
    "disabled",
    true,
  );
  expect(API.request).not.toHaveBeenCalled();
});

it("allows system installation when the target already has the required permission", async () => {
  const status = sample();
  status.packages[0].system.requires_elevation = false;
  vi.mocked(API.query).mockImplementation(async (_agent, type) => ({
    type,
    data: type === "hello" ? { capabilities: ["installation"] } : status,
  }));
  render(<Installation {...props} />);
  await ready();
  fireEvent.click(screen.getByLabelText("目标机器的所有用户"));
  fireEvent.click(screen.getByRole("button", { name: "安装当前完整包" }));
  await screen.findByText(/安装完成。/);
  expect(API.request).toHaveBeenCalledWith(
    props.agent,
    expect.objectContaining({
      data: expect.objectContaining({
        command: expect.objectContaining({
          data: expect.objectContaining({ scope: "system" }),
        }),
      }),
    }),
  );
});

it("explains a RequiresLocalAction response without sending another command", async () => {
  vi.mocked(API.request).mockRejectedValue(
    new API.ApiError("requires_local_action", "目标机需要管理员授权"),
  );
  render(<Installation {...props} />);
  fireEvent.click(await ready());
  expect((await screen.findByRole("alert")).textContent).toContain(
    "目标机需要管理员授权",
  );
  expect(screen.getByRole("alert").textContent).toContain("target-machine");
  expect(API.request).toHaveBeenCalledOnce();
});

it("keeps a running operation visible and resolves its original task", async () => {
  vi.mocked(API.request).mockResolvedValue({
    type: "task",
    data: {
      ...completed(),
      state: "running",
      message: "正在复制完整程序包",
      finished_ms: null,
    },
  });
  render(<Installation {...props} />);
  fireEvent.click(await ready());
  expect(await screen.findByText("安装任务已完成")).toBeTruthy();
  expect(API.query).toHaveBeenCalledWith(
    props.agent,
    "task",
    { operation_id: "original-operation" },
    expect.any(AbortSignal),
  );
  expect(API.request).toHaveBeenCalledOnce();
});

it("queries an uncertain operation without automatically resubmitting installation", async () => {
  vi.mocked(API.request).mockRejectedValue(
    new API.ApiError("outcome_unknown", "回复丢失"),
  );
  const original = vi.mocked(API.query).getMockImplementation()!;
  vi.mocked(API.query).mockImplementation(async (agent, type, data, signal) =>
    type === "task"
      ? {
          type,
          data: {
            ...completed((data as { operation_id: string }).operation_id),
            state: "outcome_unknown",
            message: "正在核对原安装任务",
          },
        }
      : original(agent, type, data, signal),
  );
  render(<Installation {...props} />);
  fireEvent.click(await ready());
  fireEvent.click(await screen.findByRole("button", { name: "查询安装结果" }));
  await waitFor(() =>
    expect(
      vi.mocked(API.query).mock.calls.some((call) => call[1] === "task"),
    ).toBe(true),
  );
  const command = vi.mocked(API.request).mock.calls[0][1].data as {
    operation_id: string;
  };
  expect(API.query).toHaveBeenCalledWith(
    props.agent,
    "task",
    { operation_id: command.operation_id },
    expect.any(AbortSignal),
  );
  expect(API.request).toHaveBeenCalledOnce();
  expect(screen.getByRole("button", { name: "安装当前完整包" })).toHaveProperty(
    "disabled",
    true,
  );
});

it("discards a late status after changing target", async () => {
  let resolve!: (value: { type: string; data: InstallationStatus }) => void;
  const original = vi.mocked(API.query).getMockImplementation()!;
  vi.mocked(API.query).mockImplementation(async (agent, type, data, signal) => {
    if (agent === props.agent && type === "installation_status")
      return new Promise((done) => {
        resolve = done;
      });
    if (type === "installation_status")
      return { type, data: sample({ target: "new-machine" }) };
    return original(agent, type, data, signal);
  });
  const view = render(<Installation {...props} />);
  await waitFor(() => expect(resolve).toBeDefined());
  view.rerender(<Installation {...props} agent="another-node" />);
  await ready();
  await act(async () =>
    resolve({
      type: "installation_status",
      data: sample({ target: "stale-machine" }),
    }),
  );
  expect(screen.getByText(/目标：new-machine/)).toBeTruthy();
  expect(screen.queryByText(/stale-machine/)).toBeNull();
});

it("does not display an old target's installation result on a newly selected target", async () => {
  let resolve!: (value: { type: string; data: InstallationResult }) => void;
  vi.mocked(API.request).mockReturnValue(
    new Promise((done) => {
      resolve = done;
    }),
  );
  const view = render(<Installation {...props} />);
  fireEvent.click(await ready());
  view.rerender(<Installation {...props} agent="new-install-target" />);
  await ready();
  await act(async () =>
    resolve({ type: "installation_result", data: installationResult }),
  );
  expect(screen.queryByText(/安装完成。/)).toBeNull();
  expect(screen.queryByText(/安装任务：/)).toBeNull();
});

it("recovers the original operation after leaving and reopening the installation panel", async () => {
  let finish!: (value: { type: string; data: InstallationResult }) => void;
  vi.mocked(API.request).mockReturnValue(
    new Promise((resolve) => {
      finish = resolve;
    }),
  );
  const first = render(<Installation {...props} />);
  fireEvent.click(await ready());
  const command = vi.mocked(API.request).mock.calls[0][1].data as {
    operation_id: string;
  };
  first.unmount();
  render(<Installation {...props} />);
  expect(await screen.findByText("安装任务已完成")).toBeTruthy();
  expect(API.query).toHaveBeenCalledWith(
    props.agent,
    "task",
    { operation_id: command.operation_id },
    expect.any(AbortSignal),
  );
  expect(API.request).toHaveBeenCalledOnce();
  await act(async () =>
    finish({ type: "installation_result", data: installationResult }),
  );
});

it("does not consume an offline target's first reminder or repeatedly inspect on scope changes", async () => {
  const view = render(
    <Installation {...props} online={false} active={false} />,
  );
  expect(API.query).not.toHaveBeenCalled();
  view.rerender(<Installation {...props} active={false} />);
  expect(await screen.findByRole("button", { name: "前往安装" })).toBeTruthy();
  view.rerender(<Installation {...props} active />);
  await ready();
  await waitFor(() => expect(API.query).toHaveBeenCalledTimes(4));
  fireEvent.click(screen.getByLabelText("目标机器的所有用户"));
  fireEvent.click(screen.getByLabelText("仅目标账户"));
  expect(API.query).toHaveBeenCalledTimes(4);
});

it("keeps an installed package available for explicit verification and repair", async () => {
  const status = sample();
  status.packages[0].user.installed = true;
  vi.mocked(API.query).mockImplementation(async (_agent, type) => ({
    type,
    data: type === "hello" ? { capabilities: ["installation"] } : status,
  }));
  render(<Installation {...props} />);
  expect(
    await screen.findByRole("button", { name: "校验并修复安装" }),
  ).toHaveProperty("disabled", false);
  expect(
    screen.getByText("此版本已安装，可重新校验并修复启动入口。"),
  ).toBeTruthy();
});

it("shows a failed installation task as an error and permits a deliberate retry", async () => {
  vi.mocked(API.request).mockResolvedValue({
    type: "task",
    data: { ...completed(), state: "failed", message: "复制程序文件失败" },
  });
  render(<Installation {...props} />);
  fireEvent.click(await ready());
  expect((await screen.findByRole("alert")).textContent).toContain(
    "复制程序文件失败",
  );
  expect(screen.getByRole("button", { name: "安装当前完整包" })).toHaveProperty(
    "disabled",
    false,
  );
  expect(API.request).toHaveBeenCalledOnce();
});

it("shows structured step and byte progress from the event stream while installing", async () => {
  let resolve: (value: { type: string; data: unknown }) => void = () => {};
  vi.mocked(API.request).mockImplementation(
    () =>
      new Promise<{ type: string; data: unknown }>((done) => (resolve = done)),
  );
  const view = render(<Installation {...props} />);
  fireEvent.click(await ready());
  const progressbar = await screen.findByRole("progressbar", {
    name: "安装进度",
  });
  expect(progressbar.getAttribute("aria-valuenow")).toBeNull();
  expect(screen.getByText("正在连接目标节点…")).toBeTruthy();
  const operation = (
    vi.mocked(API.request).mock.calls[0][1] as {
      data: { operation_id: string };
    }
  ).data.operation_id;
  const event = (
    step: "copying" | "verifying",
    index: number,
    current: number,
    total: number,
    operation_id = operation,
  ): API.ManagementEvent => ({
    sequence: String(index),
    operation_id,
    kind: "installation_event",
    message: "Installation progress",
    timestamp_ms: "1",
    payload: {
      type: "installation",
      data: {
        step,
        step_index: index,
        step_count: 5,
        current: String(current),
        total: String(total),
      },
    },
  });
  view.rerender(
    <Installation
      {...props}
      events={[
        event("copying", 1, 1024 * 1024, 4 * 1024 * 1024),
        event("verifying", 2, 0, 0, "another-operation"),
      ]}
    />,
  );
  expect(
    await screen.findByText("复制程序包 · 1.0 MiB / 4.0 MiB"),
  ).toBeTruthy();
  expect(screen.getByText("25%")).toBeTruthy();
  expect(
    screen
      .getByRole("progressbar", { name: "安装进度" })
      .getAttribute("aria-valuenow"),
  ).toBe("25");
  expect(screen.getByText("准备安装").className).toBe("done");
  expect(screen.getByText("复制程序包").className).toBe("active");
  expect(screen.getByText("校验已安装文件").className).toBe("pending");
  expect(screen.queryByRole("alert")).toBeNull();
  resolve({ type: "installation_result", data: installationResult });
  expect(await screen.findByText(/安装完成。/)).toBeTruthy();
});

it("localizes backend task messages", async () => {
  vi.mocked(API.request).mockResolvedValue({
    type: "task",
    data: {
      ...completed(),
      state: "running",
      message: "Executing",
      finished_ms: null,
    },
  });
  vi.mocked(API.query).mockImplementation(async (_agent, type) => {
    if (type === "hello")
      return { type: "hello", data: { capabilities: ["installation"] } };
    if (type === "installation_status") return { type, data: sample() };
    return { type, data: { ...completed(), message: "Completed" } };
  });
  render(<Installation {...props} />);
  fireEvent.click(await ready());
  expect(await screen.findByText("安装任务已完成")).toBeTruthy();
});
