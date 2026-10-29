//! UI-only translations. Configuration values, paths and raw logs are never translated.
use std::sync::atomic::{AtomicU8, Ordering};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Language {
    English,
    Simplified,
    Traditional,
}
static LANGUAGE: AtomicU8 = AtomicU8::new(1);
#[cfg(test)]
thread_local! { static TEST_LANGUAGE: std::cell::Cell<Option<Language>> = const { std::cell::Cell::new(None) }; }
impl Language {
    pub fn resolve(locale: &str) -> Self {
        let locale = locale.to_ascii_lowercase().replace('_', "-");
        if locale.starts_with("zh") {
            if ["hant", "tw", "hk", "mo"]
                .iter()
                .any(|part| locale.split(['-', '.', '@']).any(|value| value == *part))
            {
                Self::Traditional
            } else {
                Self::Simplified
            }
        } else {
            Self::English
        }
    }
}
pub fn language() -> Language {
    #[cfg(test)]
    if let Some(language) = TEST_LANGUAGE.get() {
        return language;
    }
    match LANGUAGE.load(Ordering::Relaxed) {
        0 => Language::English,
        2 => Language::Traditional,
        _ => Language::Simplified,
    }
}
#[cfg(test)]
pub fn with_language<T>(language: Language, action: impl FnOnce() -> T) -> T {
    struct Restore(Option<Language>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_LANGUAGE.set(self.0);
        }
    }
    let _restore = Restore(TEST_LANGUAGE.replace(Some(language)));
    action()
}
pub fn init() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let mut selected = None;
    while let Some(arg) = args.next() {
        if arg == "--lang" {
            let value = args
                .next()
                .ok_or_else(|| anyhow::anyhow!("--lang requires en, zh-CN or zh-TW"))?;
            anyhow::ensure!(
                ["en", "zh-cn", "zh-tw", "zh-hk", "zh-hans", "zh-hant"]
                    .contains(&value.to_ascii_lowercase().as_str()),
                "--lang requires en, zh-CN or zh-TW"
            );
            selected = Some(value);
        }
    }
    let locale = selected
        .or_else(|| {
            std::env::var("ASTRA_LANG")
                .ok()
                .filter(|value| !value.is_empty())
        })
        .or_else(|| {
            ["LC_ALL", "LC_MESSAGES", "LANG"]
                .iter()
                .find_map(|key| std::env::var(key).ok().filter(|value| !value.is_empty()))
        })
        .unwrap_or_else(system_locale);
    LANGUAGE.store(Language::resolve(&locale) as u8, Ordering::Relaxed);
    Ok(())
}
fn system_locale() -> String {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        extern "system" {
            fn GetUserDefaultLocaleName(name: *mut u16, size: i32) -> i32;
        }
        let mut name = [0u16; 85];
        let count = unsafe { GetUserDefaultLocaleName(name.as_mut_ptr(), name.len() as i32) };
        if count > 1 {
            return String::from_utf16_lossy(&name[..count as usize - 1]);
        }
    }
    "en".into()
}
pub fn t(source: &str) -> &str {
    translate(source, language())
}
pub fn translate(source: &str, language: Language) -> &str {
    let Some(entry) = CATALOG.iter().find(|entry| entry.0 == source) else {
        return source;
    };
    match language {
        Language::English => entry.1,
        Language::Simplified => entry.2,
        Language::Traditional => entry.3,
    }
}
pub fn message(source: &str, args: &[String]) -> String {
    debug_assert_eq!(
        t(source).matches("{}").count(),
        args.len(),
        "Translation argument mismatch: {source}"
    );
    let mut result = String::new();
    let mut parts = t(source).split("{}");
    result.push_str(parts.next().unwrap_or_default());
    for (index, part) in parts.enumerate() {
        if let Some(value) = args.get(index) {
            result.push_str(value);
        }
        result.push_str(part);
    }
    result
}
#[macro_export]
macro_rules! tr {
    ($source:literal $(, $arg:expr)* $(,)?) => { $crate::i18n::message($source, &[$(format!("{}", $arg)),*]) };
}
const CATALOG: &[(&str, &str, &str, &str)] = &[
    ("Opened in a new window", "Opened in a new window", "已在新窗口打开", "已在新視窗開啟"),
    ("Cannot open a new window", "Cannot open a new window", "无法打开新窗口", "無法開啟新視窗"),
    ("Use Alt+1–5 to open a page; the current draft stays here", "Use Alt+1–5 to open a page; the current draft stays here", "使用 Alt+1～5 打开页面；当前草稿保留在此窗口", "使用 Alt+1～5 開啟頁面；目前草稿保留在此視窗"),
    ("Instance unavailable: {}", "Instance unavailable: {}", "实例已不可用：{}", "實例已無法使用：{}"),

    ("F6 tasks · F7 TUI log", "F6 tasks · F7 TUI log", "F6 任务详情 · F7 TUI 日志", "F6 任務詳情 · F7 TUI 日誌"),
    ("Task details · F6 / Esc back · ↑↓ scroll", "Task details · F6 / Esc back · ↑↓ scroll", "任务进度与详情 · F6 / Esc 返回 · ↑↓ 滚动", "任務進度與詳情 · F6 / Esc 返回 · ↑↓ 捲動"),
    ("TUI log", "TUI log", "TUI 日志", "TUI 日誌"),
    ("Finished", "Finished", "已结束", "已結束"),
    ("Failed", "Failed", "失败", "失敗"),
    ("Connecting", "Connecting", "连接中", "連線中"),
    ("Validate configuration", "Validate configuration", "校验配置", "驗證設定"),
    ("Start dependency", "Start dependency", "启动依赖", "啟動相依項目"),
    ("Dependency ready", "Dependency ready", "依赖已就绪", "相依項目已就緒"),
    ("Start core", "Start core", "启动核心", "啟動核心"),
    ("Wait readiness", "Wait readiness", "等待核心就绪", "等待核心就緒"),
    ("Reload core", "Reload core", "重载核心", "重新載入核心"),
    ("Drain and stop core", "Drain and stop core", "等待连接结束并停止核心", "等待連線結束並停止核心"),
    ("Drain and stop cores", "Drain and stop cores", "并行等待连接结束并停止核心", "並行等待連線結束並停止核心"),
    ("Prepare runtime", "Prepare runtime", "准备运行环境", "準備執行環境"),
    ("Bind listeners", "Bind listeners", "绑定监听端口", "綁定監聽連接埠"),
    ("Prepare network modes", "Prepare network modes", "准备网络模式", "準備網路模式"),
    ("Commit runtime", "Commit runtime", "应用运行设置", "套用執行設定"),
    ("Read configuration", "Read configuration", "读取配置", "讀取設定"),
    ("Restart the updated manager to enable runtime controls", "Restart the updated manager to enable runtime controls", "请重启更新后的管理服务以启用运行时控制", "請重新啟動更新後的管理服務以啟用執行階段控制"),
    ("Skip authentication for CIDRs", "Skip authentication for CIDRs", "免认证的来源网段", "略過認證的來源網段"),
    ("Blocked source CIDRs", "Blocked source CIDRs", "禁止的来源网段", "封鎖的來源網段"),
    ("Allowed source CIDRs", "Allowed source CIDRs", "允许的来源网段", "允許的來源網段"),
    ("Allow LAN", "Allow LAN", "允许局域网", "允許區域網路"),
    ("Connections", "Connections", "实时连接", "即時連線"),
    ("Upload", "Upload", "上传", "上傳"),
    ("Download", "Download", "下载", "下載"),
    ("Total", "Total", "累计", "累計"),
    ("Rule", "Rule", "规则", "規則"),
    ("Global", "Global", "全局", "全域"),
    ("members", "members", "个成员", "個成員"),
    ("Mixed listener", "Mixed listener", "混合监听地址", "混合監聽位址"),
    ("Inbound authentication", "Inbound authentication", "入站认证（用户名:密码）", "入站認證（使用者名稱:密碼）"),
    ("Applying runtime change", "Applying runtime change", "正在应用运行时变更", "正在套用執行階段變更"),
    ("Group test started", "Group test started", "已开始代理组测速", "已開始代理組測速"),
    ("Runtime updated; profile unchanged", "Runtime updated; profile unchanged", "运行时设置已更新；配置文件未改动", "執行階段設定已更新；設定檔未變更"),
    ("Runtime unavailable; update or restart the proxy core", "Runtime unavailable; update or restart the proxy core", "运行时接口不可用，请更新或重启代理核心", "執行階段介面無法使用，請更新或重新啟動代理核心"),
    ("Select a running proxy in Profiles", "Select a running proxy in Profiles", "请在配置页选择已运行的代理实例", "請在設定頁選取執行中的代理實例"),
    ("Enter members · t test · m mode · S system proxy · T TUN", "Enter members · t test · m mode · S system proxy · T TUN", "Enter 查看成员 · t 测速 · m 模式 · S 系统代理 · T TUN", "Enter 檢視成員 · t 測速 · m 模式 · S 系統代理 · T TUN"),
    ("/ filter · d close · D close all", "/ filter · d close · D close all", "/ 筛选 · d 关闭选中连接 · D 关闭全部", "/ 篩選 · d 關閉選取連線 · D 關閉全部"),
    ("Enter select · t test · Esc back", "Enter select · t test · Esc back", "Enter 选择 · t 测速 · Esc 返回", "Enter 選取 · t 測速 · Esc 返回"),
    ("Close all", "Close all", "关闭全部", "關閉全部"),
    ("Close selected", "Close selected", "关闭选中", "關閉選取"),
    ("Test", "Test", "测速", "測速"),
    ("Validated and saved; hot applied when running; previous file in state directory backups/", "Validated and saved; hot applied when running; previous file in state directory backups/", "已校验并保存；运行中已热应用；原文件备份在状态目录 backups/", "已驗證並儲存；執行中已熱套用；原檔備份在狀態目錄 backups/"),
    ("Dashboard", "Dashboard", "仪表盘", "儀表板"),
    ("Proxies", "Proxies", "代理", "代理"),
    ("Profiles", "Profiles", "配置", "設定"),
    ("Live logs", "Live logs", "实时日志", "即時日誌"),
    ("Tools", "Tools", "工具", "工具"),
    ("Run time", "Run time", "运行时间", "執行時間"),
    ("Running instances", "Running instances", "运行实例", "執行中實例"),
    ("Proxy instances", "Proxy instances", "代理实例", "代理實例"),
    ("Rule instances", "Rule instances", "规则实例", "規則實例"),
    ("No instance selected", "No instance selected", "未选择实例", "未選取實例"),
    ("Profile", "Profile", "配置文件", "設定檔"),
    ("Listeners / Outbounds", "Listeners / Outbounds", "监听与出站", "監聽與出站"),
    ("Outbounds", "Outbounds", "出站", "出站"),
    ("Enlarge terminal to view dashboard", "Enlarge terminal to view dashboard", "请放大终端查看仪表盘", "請放大終端機檢視儀表板"),
    ("Tab core", "Tab core", "Tab 切换核心", "Tab 切換核心"),
    ("Esc back", "Esc back", "Esc 返回", "Esc 返回"),
    ("1–5 pages · Shift-Tab previous · Tab core · ↑↓ select · Enter actions · q quit · ? help", "1–5 pages · Shift-Tab previous · Tab core · ↑↓ select · Enter actions · q quit · ? help", "1–5 页面 · Shift-Tab 上一页 · Tab 核心 · ↑↓ 选择 · Enter 操作 · q 退出 · ? 帮助", "1–5 頁面 · Shift-Tab 上一頁 · Tab 核心 · ↑↓ 選取 · Enter 操作 · q 離開 · ? 說明"),
    ("Node tests", "Node tests", "节点测试", "節點測試"),
    ("Node", "Node", "节点", "節點"),
    ("TCP latency", "TCP latency", "TCP 延迟", "TCP 延遲"),
    ("Connectivity", "Connectivity", "连通性", "連線狀態"),
    ("Exit IP", "Exit IP", "出口 IP", "出口 IP"),
    ("Connected", "Connected", "连通", "已連線"),
    ("Testing…", "Testing…", "测试中…", "測試中…"),
    ("Queued", "Queued", "排队中", "排隊中"),
    ("Querying exit IP…", "Querying exit IP…", "查询出口 IP…", "查詢出口 IP…"),
    ("Progress {}/{} · Running {} · Queued {} · {}s", "Progress {}/{} · Running {} · Queued {} · {}s", "进度 {}/{} · 测试中 {} · 排队 {} · {}秒", "進度 {}/{} · 測試中 {} · 排隊 {} · {}秒"),
    ("↑↓ select · Space toggle · a all/none · c clear\nEnter/t test selected · x stop · Esc back", "↑↓ select · Space toggle · a all/none · c clear\nEnter/t test selected · x stop · Esc back", "↑↓ 选择 · 空格勾选 · a 全选/取消全选 · c 清空\nEnter/t 测试所选节点 · x 停止测试 · Esc 返回", "↑↓ 選擇 · 空白鍵勾選 · a 全選/取消全選 · c 清除\nEnter/t 測試所選節點 · x 停止測試 · Esc 返回"),
    ("Loading nodes…", "Loading nodes…", "正在加载节点…", "正在載入節點…"),
    ("Select at least one node", "Select at least one node", "请至少选择一个节点", "請至少選擇一個節點"),
    ("Select nodes, then press Enter to test", "Select nodes, then press Enter to test", "选择节点后按 Enter 开始测试", "選擇節點後按 Enter 開始測試"),
    ("Include in Proxy group", "Include in Proxy group", "加入 Proxy 组", "加入 Proxy 群組"),
    ("↑↓ select · Space toggle · a all/none · c clear\nEnter/t test selected · Esc back", "↑↓ select · Space toggle · a all/none · c clear\nEnter/t test selected · Esc back", "↑↓ 选择 · 空格勾选 · a 全选/取消全选 · c 清空\nEnter/t 测试所选节点 · Esc 返回", "↑↓ 選擇 · 空白鍵勾選 · a 全選/取消全選 · c 清除\nEnter/t 測試所選節點 · Esc 返回"),

    ("hjkl scroll · gg/G first/follow · / search · v level\nf filter · m memory · L capture · Space pause · Esc back", "hjkl scroll · gg/G first/follow · / search · v level\nf filter · m memory · L capture · Space pause · Esc back", "hjkl 滚动 · gg/G 开头/跟随 · / 搜索 · v 级别\nf 过滤 · m 内存 · L 记录级别 · 空格 暂停 · Esc 返回", "hjkl 捲動 · gg/G 開頭/跟隨 · / 搜尋 · v 級別\nf 篩選 · m 記憶體 · L 記錄級別 · 空白 暫停 · Esc 返回"),
    ("hjkl navigate · gg/G ends · zM/zR fold/open · Ctrl-S save\nn add · r rename set · e toggle set · Ctrl-T query", "hjkl navigate · gg/G ends · zM/zR fold/open · Ctrl-S save\nn add · r rename set · e toggle set · Ctrl-T query", "hjkl 导航 · gg/G 首尾 · zM/zR 折叠/展开 · Ctrl-S 保存\nn 新增 · r 重命名规则集 · e 启停规则集 · Ctrl-T 查询", "hjkl 導航 · gg/G 首尾 · zM/zR 收合/展開 · Ctrl-S 儲存\nn 新增 · r 重新命名規則集 · e 啟停規則集 · Ctrl-T 查詢"),
    ("hjkl navigate · gg/G ends · Ctrl-S save · Esc back\nn add · d delete · K/J move · Ctrl-O import · F2 reveal", "hjkl navigate · gg/G ends · Ctrl-S save · Esc back\nn add · d delete · K/J move · Ctrl-O import · F2 reveal", "hjkl 导航 · gg/G 首尾 · Ctrl-S 保存 · Esc 返回\nn 新增 · d 删除 · K/J 移动 · Ctrl-O 导入 · F2 显示凭据", "hjkl 導航 · gg/G 首尾 · Ctrl-S 儲存 · Esc 返回\nn 新增 · d 刪除 · K/J 移動 · Ctrl-O 匯入 · F2 顯示認證資訊"),
    ("j/k move · h/l tabs · gg/G ends · Enter actions\nn add · e edit · L logs · t test · q quit · Q stop all", "j/k move · h/l tabs · gg/G ends · Enter actions\nn add · e edit · L logs · t test · q quit · Q stop all", "j/k 移动 · h/l 切页 · gg/G 首尾 · Enter 操作\nn 新增 · e 编辑 · L 日志 · t 检测 · q 退出 · Q 停止全部", "j/k 移動 · h/l 切頁 · gg/G 首尾 · Enter 操作\nn 新增 · e 編輯 · L 日誌 · t 檢測 · q 離開 · Q 停止全部"),
    ("j/k move · h/l tabs · gg/G ends · Enter actions\nn add · e edit · L logs · t query · q quit · Q stop all", "j/k move · h/l tabs · gg/G ends · Enter actions\nn add · e edit · L logs · t query · q quit · Q stop all", "j/k 移动 · h/l 切页 · gg/G 首尾 · Enter 操作\nn 新增 · e 编辑 · L 日志 · t 查询 · q 退出 · Q 停止全部", "j/k 移動 · h/l 切頁 · gg/G 首尾 · Enter 操作\nn 新增 · e 編輯 · L 日誌 · t 查詢 · q 離開 · Q 停止全部"),
    ("Domain rules", "Domain rules", "域名规则", "網域規則"),
    ("IP rules", "IP rules", "IP / GEOIP 规则", "IP / GEOIP 規則"),
    ("Process rules", "Process rules", "进程规则", "程序規則"),
    ("Rule set references", "Rule set references", "规则集引用", "規則集引用"),
    ("Network / port rules", "Network / port rules", "网络 / 端口规则", "網路 / 連接埠規則"),
    ("Other rules", "Other rules", "其他规则", "其他規則"),
    ("j/k move · h/l fold/open · gg/G first/last · zM/zR fold/open all", "j/k move · h/l fold/open · gg/G first/last · zM/zR fold/open all", "j/k 移动 · h/l 收起/展开 · gg/G 首尾 · zM/zR 全部收起/展开", "j/k 移動 · h/l 收合/展開 · gg/G 首尾 · zM/zR 全部收合/展開"),
    ("Rule set enabled", "Enabled", "启用", "啟用"),
    ("Configuration · {}{}", "Configuration · {}{}", "配置编辑 · {}{}", "設定編輯 · {}{}"),
    ("Enter expand/collapse · r rename · e enable/disable · d delete", "Enter expand/collapse · r rename · e enable/disable · d delete", "Enter 展开/收起 · r 重命名 · e 启停 · d 删除", "Enter 展開/收合 · r 重新命名 · e 啟停 · d 刪除"),
    ("Matched condition: {} {}", "Matched condition: {} {}", "具体匹配项：{} {}", "具體符合項目：{} {}"),
    ("Route explanation", "Route explanation", "规则命中查询", "規則命中查詢"),
    ("Proxy groups", "Proxy groups", "代理组", "代理組"),
    ("Import subscription groups", "Import subscription groups", "导入订阅代理组", "匯入訂閱代理組"),
    ("Enter add subscription · n add · d remove · Ctrl-S save and update", "Enter add subscription · n add · d remove · Ctrl-S save and update", "Enter 添加订阅 · n 新增 · d 删除 · Ctrl-S 保存并更新", "Enter 新增訂閱 · n 新增 · d 刪除 · Ctrl-S 儲存並更新"),
    ("Import node links", "Import node links", "导入节点链接", "匯入節點連結"),
    ("Import TOML / YAML or vless:// link", "Import TOML / YAML or vless:// link", "导入 TOML / YAML 文件或 vless:// 链接", "匯入 TOML / YAML 檔案或 vless:// 連結"),
    ("Import config or proxy share links", "Import config or proxy share links", "导入配置文件或代理分享链接", "匯入設定檔或代理分享連結"),
    ("All operations", "All operations", "全部操作", "全部操作"),
    ("Available actions", "Available actions", "当前可用操作", "目前可用操作"),
    ("↑↓ scroll · ? / Esc close", "↑↓ scroll · ? / Esc close", "↑↓ 滚动 · ? / Esc 关闭", "↑↓ 捲動 · ? / Esc 關閉"),
    ("↑↓ scroll · Space pause · w wrap · Esc back · ? all operations", "↑↓ scroll · Space pause · w wrap · Esc back · ? all operations", "↑↓ 滚动 · 空格暂停 · w 换行 · Esc 返回 · ? 全部操作", "↑↓ 捲動 · 空白鍵暫停 · w 換行 · Esc 返回 · ? 全部操作"),
    ("Ctrl-O import · F4 external editor · F5 read draft · Ctrl-C leave, keep draft", "Ctrl-O import · F4 external editor · F5 read draft · Ctrl-C leave, keep draft", "Ctrl-O 导入 · F4 外部编辑器 · F5 重读草稿 · Ctrl-C 离开并保留草稿", "Ctrl-O 匯入 · F4 外部編輯器 · F5 重讀草稿 · Ctrl-C 離開並保留草稿"),
    ("↑↓ select · Enter actions · e edit · q quit · ? all operations", "↑↓ select · Enter actions · e edit · q quit · ? all operations", "↑↓ 选择 · Enter 操作 · e 编辑 · q 退出 · ? 全部操作", "↑↓ 選擇 · Enter 操作 · e 編輯 · q 離開 · ? 全部操作"),
    ("Log wrap", "Log wrap", "日志自动换行", "日誌自動換行"),
    ("On", "On", "开启", "開啟"),
    ("Off", "Off", "关闭", "關閉"),
    ("State", "State", "状态", "狀態"),
    ("{}\n{}/{} lines · limit {} · released {}\n过滤: {}\n搜索: {}\nLocal log: {}", "{}\n{}/{} lines · limit {} · released {}\nFilter: {}\nSearch: {}\nLocal log: {}", "{}\n{}/{} 条 · 内存上限 {} · 已释放 {}\n过滤: {}\n搜索: {}\n本地日志: {}", "{}\n{}/{} 筆 · 記憶體上限 {} · 已釋放 {}\n篩選: {}\n搜尋: {}\n本機日誌: {}"),
    ("Operation status", "Operation status", "操作状态", "操作狀態"),
    ("Checking saved configuration", "Checking saved configuration", "检查已保存的配置", "檢查已儲存的設定"),
    ("Waiting for manager confirmation", "Waiting for manager confirmation", "等待管理器确认", "等待管理員確認"),
    ("Operation failed; see details above", "Operation failed; see details above", "操作失败，详细原因见上方", "操作失敗，詳細原因見上方"),
    ("Ready for editing", "Ready for editing", "已就绪，可编辑配置", "已就緒，可編輯設定"),
    ("Completed", "Completed", "处理完成", "處理完成"),
    ("Processing request", "Processing request", "正在处理请求", "正在處理請求"),
    ("Page operations", "Page operations", "本页操作", "本頁操作"),
    ("Subscription nodes ({})", "Subscription nodes ({})", "订阅节点（{}）", "訂閱節點（{}）"),
    ("No nodes available · save and update subscription", "No nodes available · save and update subscription", "暂无节点 · 保存并更新订阅后显示", "暫無節點 · 儲存並更新訂閱後顯示"),
    ("Subscriptions", "Subscriptions", "订阅列表", "訂閱列表"),
    ("Update subscriptions", "Update subscriptions", "立即更新订阅", "立即更新訂閱"),
    ("Subscription update scheduled", "Subscription update scheduled", "已触发订阅更新，结果见日志", "已觸發訂閱更新，結果見日誌"),
    ("Enter a subscription name", "Enter a subscription name", "请输入订阅名称", "請輸入訂閱名稱"),
    ("Subscription name already exists", "Subscription name already exists", "订阅名称已存在", "訂閱名稱已存在"),
    ("Subscription name: {}▏\nEnter confirm · Esc cancel", "Subscription name: {}▏\nEnter confirm · Esc cancel", "订阅名称：{}▏\nEnter 确认 · Esc 取消", "訂閱名稱：{}▏\nEnter 確認 · Esc 取消"),
    ("Subscription", "Subscription", "订阅", "訂閱"),
    ("Subscription URL", "Subscription URL", "订阅链接", "訂閱連結"),
    ("Update interval (seconds)", "Update interval (seconds)", "更新间隔（秒）", "更新間隔（秒）"),
    ("Group mode", "Group mode", "选择方式", "選擇方式"),
    ("Group members", "Group members", "组内成员", "組內成員"),
    ("Selected member", "Selected member", "手动选择", "手動選擇"),
    ("Test URL", "Test URL", "测速地址", "測速位址"),
    ("Test interval (seconds)", "Test interval (seconds)", "测速间隔（秒）", "測速間隔（秒）"),
    ("Test timeout (ms)", "Test timeout (ms)", "测速超时（毫秒）", "測速逾時（毫秒）"),
    ("Switch tolerance (ms)", "Switch tolerance (ms)", "切换阈值（毫秒）", "切換閾值（毫秒）"),
    ("Automatic", "Automatic", "自动选择", "自動選擇"),
    ("Proxy target name already exists", "Proxy target name already exists", "节点或代理组名称已存在", "節點或代理組名稱已存在"),
    ("Select a member of this group", "Select a member of this group", "请选择当前代理组的成员", "請選擇目前代理組的成員"),
    ("Choose at least one group member", "Choose at least one group member", "请至少选择一个成员", "請至少選擇一個成員"),
    ("Enter a proxy group name", "Enter a proxy group name", "请输入有效的代理组名称", "請輸入有效的代理組名稱"),
    ("Proxy target is referenced; change its groups or default before deleting", "Proxy target is referenced; change its groups or default before deleting", "仍被代理组或默认出站引用，请先修改引用", "仍被代理組或預設出站引用，請先修改引用"),
    (" · {} members · {} → {}", " · {} members · {} → {}", " · {} 个成员 · {} → {}", " · {} 個成員 · {} → {}"),
    ("Enter edit/fold · n new group · r rename · d delete · Ctrl-S apply", "Enter edit/fold · n new group · r rename · d delete · Ctrl-S apply", "Enter 编辑/折叠 · n 新建组 · r 改名 · d 删除 · Ctrl-S 应用", "Enter 編輯/摺疊 · n 新增組 · r 改名 · d 刪除 · Ctrl-S 套用"),
    ("j/k move · Space toggle · Enter confirm · Esc cancel", "j/k move · Space toggle · Enter confirm · Esc cancel", "j/k 移动 · 空格勾选 · Enter 确认 · Esc 取消", "j/k 移動 · 空白鍵勾選 · Enter 確認 · Esc 取消"),
    ("Proxy group name: {}▏\nEnter confirm · Esc cancel", "Proxy group name: {}▏\nEnter confirm · Esc cancel", "代理组名称：{}▏\nEnter 确认 · Esc 取消", "代理組名稱：{}▏\nEnter 確認 · Esc 取消"),
    ("Disconnect on manual switch", "Disconnect on manual switch", "手动切换时断开旧连接", "手動切換時中斷舊連線"),
    ("Disconnect on automatic improvement", "Disconnect on automatic improvement", "自动优选时断开旧连接", "自動優選時中斷舊連線"),
    ("Disconnect on member failure", "Disconnect on member failure", "节点故障时断开旧连接", "節點故障時中斷舊連線"),
    ("Disconnect on availability recovery", "Disconnect on availability recovery", "恢复可用时断开旧连接", "恢復可用時中斷舊連線"),
    ("Disconnect on members or mode change", "Disconnect on members or mode change", "成员或模式变更时断开旧连接", "成員或模式變更時中斷舊連線"),
    ("Include all nodes", "Include all nodes", "动态包含所有节点", "動態包含所有節點"),
    ("Include other groups", "Include other groups", "动态包含其他代理组", "動態包含其他代理組"),
    ("Dynamic member filter", "Dynamic member filter", "动态成员筛选（正则）", "動態成員篩選（正則）"),
    ("Dynamic member exclusion", "Dynamic member exclusion", "动态成员排除（正则）", "動態成員排除（正則）"),
    ("Built-in groups PROXY, DIRECT and REJECT cannot be renamed or deleted", "Built-in groups PROXY, DIRECT and REJECT cannot be renamed or deleted", "内置组 PROXY、DIRECT、REJECT 不能重命名或删除，可编辑其成员", "內建群組 PROXY、DIRECT、REJECT 不能重新命名或刪除，可編輯其成員"),
    ("External draft exceeds 480 KiB", "External draft exceeds 480 KiB", "外部草稿超过 480 KiB", "外部草稿超過 480 KiB"),
    ("External draft must be UTF-8", "External draft must be UTF-8", "外部草稿必须使用 UTF-8 编码", "外部草稿必須使用 UTF-8 編碼"),
    ("Invalid editor command; use a JSON argument list", "Invalid editor command; use a JSON argument list", "编辑器命令无效，可使用 JSON 参数列表", "編輯器命令無效，可使用 JSON 參數列表"),
    ("Unclosed quote in editor command", "Unclosed quote in editor command", "编辑器命令中的引号未闭合", "編輯器命令中的引號未閉合"),
    ("Invalid editor command", "Invalid editor command", "编辑器命令无效", "編輯器命令無效"),
    ("Editor command must be Unicode", "Editor command must be Unicode", "编辑器命令必须是有效文本", "編輯器命令必須是有效文字"),
    ("Cannot start external editor", "Cannot start external editor", "无法启动外部编辑器", "無法啟動外部編輯器"),
    ("External editor exited unsuccessfully", "External editor exited unsuccessfully", "外部编辑器异常退出", "外部編輯器異常結束"),
    ("Open a configuration first", "Open a configuration first", "请先打开配置编辑页", "請先開啟配置編輯頁"),
    ("No external draft; press F4 to open an editor", "No external draft; press F4 to open an editor", "没有外部草稿，请按 F4 打开编辑器", "沒有外部草稿，請按 F4 開啟編輯器"),
    ("External draft belongs to another instance", "External draft belongs to another instance", "外部草稿属于其他实例", "外部草稿屬於其他實例"),
    ("TUI draft changed; external file is kept for manual import", "TUI draft changed; external file is kept for manual import", "TUI 草稿已修改；外部文件已保留，可通过导入手动合并", "TUI 草稿已修改；外部檔案已保留，可透過匯入手動合併"),
    ("External draft: {} · F5 read again · Ctrl-S save", "External draft: {} · F5 read again · Ctrl-S save", "外部草稿：{} · F5 重读 · Ctrl-S 保存", "外部草稿：{} · F5 重讀 · Ctrl-S 儲存"),
    ("Confirm or cancel the field before reading external changes", "Confirm or cancel the field before reading external changes", "请先确认或取消当前字段，再读取外部修改", "請先確認或取消目前欄位，再讀取外部修改"),
    ("Cannot open editor until the draft is checkpointed", "Cannot open editor until the draft is checkpointed", "草稿恢复记录保存失败，暂不能打开外部编辑器", "草稿恢復記錄儲存失敗，暫不能開啟外部編輯器"),
    ("F4 external editor · F5 read external changes · Ctrl-S save", "F4 external editor · F5 read external changes · Ctrl-S save", "F4 外部编辑器 · F5 读取外部修改 · Ctrl-S 保存", "F4 外部編輯器 · F5 讀取外部修改 · Ctrl-S 儲存"),
    ("Create a rule set before adding a reference", "Create a rule set before adding a reference", "请先创建规则集，再添加引用", "請先建立規則集，再新增引用"),
    ("Invalid process name or absolute path", "Invalid process name or absolute path", "请输入有效的进程名或可执行文件绝对路径", "請輸入有效的程序名稱或執行檔絕對路徑"),
    ("Enter host[:port] [tcp|udp] [process name or path]", "Enter host[:port] [tcp|udp] [process name or path]", "输入 域名或IP[:端口] [tcp|udp] [进程名或路径]", "輸入 網域或IP[:連接埠] [tcp|udp] [程序名稱或路徑]"),
    ("Enter host[:port] [tcp|udp] [process name]", "Enter host[:port] [tcp|udp] [process name]", "输入 域名或IP[:端口] [tcp|udp] [进程名]", "輸入 網域或IP[:連接埠] [tcp|udp] [程序名稱]"),
    ("Use [IPv6]:port for an IPv6 target", "Use [IPv6]:port for an IPv6 target", "IPv6 带端口时请使用 [IPv6]:端口", "IPv6 含連接埠時請使用 [IPv6]:連接埠"),
    ("Invalid target port", "Invalid target port", "目标端口无效", "目標連接埠無效"),
    ("Invalid route query target", "Invalid route query target", "查询目标无效", "查詢目標無效"),
    ("Target: {}:{} · {}", "Target: {}:{} · {}", "目标：{}:{} · {}", "目標：{}:{} · {}"),
    ("Process: {}", "Process: {}", "进程：{}", "程序：{}"),
    ("Actual action: {}", "Actual action: {}", "实际动作：{}", "實際動作：{}"),
    ("Matched path: {}", "Matched path: {}", "命中路径：{}", "命中路徑：{}"),
    ("Default action: {}", "Default action: {}", "默认动作：{}", "預設動作：{}"),
    ("Running core: PID {} · revision {}", "Running core: PID {} · revision {}", "运行核心：PID {} · 规则版本 {}", "執行核心：PID {} · 規則版本 {}"),
    ("Explicit rules take precedence over the default action", "Explicit rules take precedence over the default action", "显式规则优先于默认动作", "明確規則優先於預設動作"),
    ("\"{}\" has no domain suffix, so it is routed as a local hostname (geosite private matches every name without a dot); query a full domain such as {}.com", "\"{}\" has no domain suffix, so it is routed as a local hostname (geosite private matches every name without a dot); query a full domain such as {}.com", "“{}”没有域名后缀，会按本地主机名匹配（geosite private 匹配所有不含点的名称）；请查询完整域名，如 {}.com", "「{}」沒有網域後綴，會按本機主機名稱比對（geosite private 比對所有不含點的名稱）；請查詢完整網域，如 {}.com"),
    ("Query uses active rules; no traffic is forwarded", "Query uses active rules; no traffic is forwarded", "使用运行中的规则查询，不发起网络连接", "使用執行中的規則查詢，不建立網路連線"),
    ("↑↓ scroll · Esc close", "↑↓ scroll · Esc close", "↑↓ 滚动 · Esc 关闭", "↑↓ 捲動 · Esc 關閉"),
    ("Restart manager to enable route explanation", "Restart manager to enable route explanation", "请更新并重启管理服务以启用命中查询", "請更新並重新啟動管理服務以啟用命中查詢"),
    ("Select a rule instance", "Select a rule instance", "请选择规则实例", "請選擇規則執行個體"),
    ("Start the rule instance before querying a route", "Start the rule instance before querying a route", "请先启动规则实例再查询", "請先啟動規則執行個體再查詢"),
    ("Enabled", "Enabled", "已启用", "已啟用"),
    ("Disabled", "Disabled", "已停用", "已停用"),
    ("inline · {} entries", "inline · {} entries", "内联 · {} 条", "內嵌 · {} 條"),
    (" · {} · {} · {} · {} references", " · {} · {} · {} · {} references", " · {} · {} · {} · {} 处引用", " · {} · {} · {} · {} 處引用"),
    ("Rule set is referenced; change its rules before deleting", "Rule set is referenced; change its rules before deleting", "该规则集仍被引用，请先修改引用它的规则", "此規則集仍被引用，請先修改引用它的規則"),
    ("r rename · e enable/disable · d delete · Ctrl-S save", "r rename · e enable/disable · d delete · Ctrl-S save", "r 重命名 · e 启停 · d 删除 · Ctrl-S 保存", "r 重新命名 · e 啟停 · d 刪除 · Ctrl-S 儲存"),
    ("Rename rule set: {}▏\nEnter confirm · Esc cancel", "Rename rule set: {}▏\nEnter confirm · Esc cancel", "规则集新名称：{}▏\nEnter 确认 · Esc 取消", "規則集新名稱：{}▏\nEnter 確認 · Esc 取消"),
    ("Enter edit · Ctrl-S save · Ctrl-T query · Esc back\nn add · r rename set · e toggle set · d delete", "Enter edit · Ctrl-S save · Ctrl-T query · Esc back\nn add · r rename set · e toggle set · d delete", "Enter 编辑 · Ctrl-S 保存 · Ctrl-T 命中查询 · Esc 返回\nn 新增 · r 重命名规则集 · e 启停规则集 · d 删除", "Enter 編輯 · Ctrl-S 儲存 · Ctrl-T 命中查詢 · Esc 返回\nn 新增 · r 重新命名規則集 · e 啟停規則集 · d 刪除"),
    ("Enter actions · s start · x stop · e edit · l logs · t query\nb bindings · n add · Tab switch · q quit · Q stop all", "Enter actions · s start · x stop · e edit · l logs · t query\nb bindings · n add · Tab switch · q quit · Q stop all", "Enter 操作 · s 启动 · x 停止 · e 编辑 · l 日志 · t 命中查询\nb 启动绑定 · n 新增 · Tab 切换 · q 退出 · Q 停止全部", "Enter 操作 · s 啟動 · x 停止 · e 編輯 · l 日誌 · t 命中查詢\nb 啟動綁定 · n 新增 · Tab 切換 · q 離開 · Q 停止全部"),
    ("Type to filter · ↑↓ select · Tab complete · Enter confirm · Esc cancel", "Type to filter · ↑↓ select · Tab complete · Enter confirm · Esc cancel", "输入筛选 · ↑↓ 选择 · Tab 补全 · Enter 确认 · Esc 取消", "輸入篩選 · ↑↓ 選擇 · Tab 補全 · Enter 確認 · Esc 取消"),
    ("Enter confirm · Esc cancel · Ctrl-U clear · F2 reveal", "Enter confirm · Esc cancel · Ctrl-U clear · F2 reveal", "Enter 确认 · Esc 取消 · Ctrl-U 清空 · F2 显示凭据", "Enter 確認 · Esc 取消 · Ctrl-U 清除 · F2 顯示認證資訊"),
    ("No matches", "No matches", "无匹配项", "無符合項目"),
    ("Select direct / block / a proxy node; manual proxy:tag is supported", "Select direct / block / a proxy node; manual proxy:tag is supported", "选择直连、拦截或代理节点；也可手动填写 proxy:标签", "選擇直連、封鎖或代理節點；亦可手動填寫 proxy:標籤"),
    ("Select an existing node; leave empty to use the only node", "Select an existing node; leave empty to use the only node", "选择已有节点；只有一个节点时可留空", "選擇現有節點；只有一個節點時可留空"),
    ("Node suggestions unavailable; manual entry is supported", "Node suggestions unavailable; manual entry is supported", "节点建议暂不可用，可手动填写", "節點建議暫時無法使用，可手動填寫"),
    ("Connectivity / Exit IP", "Connectivity / Exit IP", "连通性 / 出口 IP", "連線測試 / 出口 IP"),
    ("Connectivity / Exit IP · {}", "Connectivity / Exit IP · {}", "连通性 / 出口 IP · {}", "連線測試 / 出口 IP · {}"),
    ("Checking DIRECT and proxy exits… · Esc cancel", "Checking DIRECT and proxy exits… · Esc cancel", "正在检查 DIRECT 和代理出口… · Esc 取消", "正在檢查 DIRECT 與代理出口… · Esc 取消"),
    ("Using running configuration", "Using running configuration", "使用运行中的配置", "使用執行中的設定"),
    ("Using saved configuration · instance stopped", "Using saved configuration · instance stopped", "使用已保存配置 · 实例未运行", "使用已儲存設定 · 執行個體未執行"),
    ("Proxy · {}", "Proxy · {}", "代理 · {}", "代理 · {}"),
    ("Connected · {} ms · Exit IP {}", "Connected · {} ms · Exit IP {}", "连通 · {} 毫秒 · 出口 IP {}", "連線成功 · {} 毫秒 · 出口 IP {}"),
    ("Connected · {} ms · IP check failed", "Connected · {} ms · IP check failed", "连接成功 · {} 毫秒 · IP 检查失败", "連線成功 · {} 毫秒 · IP 檢查失敗"),
    ("Connection failed", "Connection failed", "连接失败", "連線失敗"),
    ("Diagnostics interrupted", "Diagnostics interrupted", "检测已中断", "檢測已中斷"),
    ("↑↓ scroll · t retry · Esc close", "↑↓ scroll · t retry · Esc close", "↑↓ 滚动 · t 重试 · Esc 关闭", "↑↓ 捲動 · t 重試 · Esc 關閉"),
    ("Restart manager to enable diagnostics", "Restart manager to enable diagnostics", "重启管理服务后可使用出口检查", "重新啟動管理服務後可檢查出口"),
    ("Enter a line limit from 1 to 100000", "Enter a line limit from 1 to 100000", "请输入 1–100000 之间的行数", "請輸入 1–100000 之間的行數"),
    ("Log memory limit updated", "Log memory limit updated", "日志内存上限已更新", "日誌記憶體上限已更新"),
    ("In-memory log lines · 1–100000", "In-memory log lines · 1–100000", "内存日志条数 · 1–100000", "記憶體日誌筆數 · 1–100000"),
    ("{} · {}/{} lines · limit {} · released {}\n过滤: {}  搜索: {}\nLocal log: {}", "{} · {}/{} lines · limit {} · released {}\nFilter: {}  Search: {}\nLocal log: {}", "{} · 显示 {}/{} 条 · 上限 {} · 已释放 {}\n过滤：{}  搜索：{}\n本地日志：{}", "{} · 顯示 {}/{} 筆 · 上限 {} · 已釋放 {}\n篩選：{}  搜尋：{}\n本機日誌：{}"),
    ("/ search · f filter · l level · m memory · Space pause\nn/N next/previous · L capture · r reconnect · Esc back", "/ search · f filter · l level · m memory · Space pause\nn/N next/previous · L capture · r reconnect · Esc back", "/ 搜索 · f 过滤 · l 级别 · m 内存上限 · 空格 暂停\nn/N 跳转 · L 记录级别 · r 重连 · Esc 返回", "/ 搜尋 · f 篩選 · l 級別 · m 記憶體上限 · 空白 暫停\nn/N 跳轉 · L 記錄級別 · r 重連 · Esc 返回"),
    ("Enter actions · s start · x stop · e edit · l logs · t test\nb bindings · n add · Tab switch · q quit · Q stop all", "Enter actions · s start · x stop · e edit · l logs · t test\nb bindings · n add · Tab switch · q quit · Q stop all", "Enter 操作 · s 启动 · x 停止 · e 编辑 · l 日志 · t 检测\nb 绑定 · n 新增 · Tab 切换 · q 退出 · Q 停止全部", "Enter 操作 · s 啟動 · x 停止 · e 編輯 · l 日誌 · t 檢測\nb 繫結 · n 新增 · Tab 切換 · q 退出 · Q 停止全部"),
    ("Confirm or cancel the field before importing", "Confirm or cancel the field before importing", "请先确认或取消当前字段，再导入", "請先確認或取消目前欄位，再匯入"),
    ("Import TOML / YAML · paste or drop a file path", "Import TOML / YAML · paste or drop a file path", "导入 TOML / YAML · 粘贴或拖入文件路径", "匯入 TOML / YAML · 貼上或拖入檔案路徑"),
    ("Replace editor draft?", "Replace editor draft?", "替换当前草稿？", "取代目前草稿？"),
    ("Unsaved draft will be replaced", "Unsaved draft will be replaced", "未保存的草稿将被替换", "未儲存的草稿將被取代"),
    ("Imported to draft · Ctrl-S save", "Imported to draft · Ctrl-S save", "已导入草稿 · Ctrl-S 保存", "已匯入草稿 · Ctrl-S 儲存"),
    ("Choose one TOML or YAML file", "Choose one TOML or YAML file", "请选择一个 TOML 或 YAML 文件", "請選擇一個 TOML 或 YAML 檔案"),
    ("Configuration exceeds 480 KiB", "Configuration exceeds 480 KiB", "配置超过 480 KiB", "設定超過 480 KiB"),
    ("Configuration must be UTF-8", "Configuration must be UTF-8", "配置必须为 UTF-8 编码", "設定必須為 UTF-8 編碼"),
    ("Invalid configuration", "Invalid configuration", "配置格式无效", "設定格式無效"),
    ("Configuration does not match this core", "Configuration does not match this core", "配置与当前 Core 类型不匹配", "設定與目前 Core 類型不符"),
    ("Enter confirm · Esc cancel · ↑↓ scroll", "Enter confirm · Esc cancel · ↑↓ scroll", "Enter 确认 · Esc 取消 · ↑↓ 滚动", "Enter 確認 · Esc 取消 · ↑↓ 捲動"),
    ("Enter edit · Ctrl-S save · Ctrl-O import · Esc back\nn add · d delete · K/J move · F2 reveal", "Enter edit · Ctrl-S save · Ctrl-O import · Esc back\nn add · d delete · K/J move · F2 reveal", "Enter 编辑 · Ctrl-S 保存 · Ctrl-O 导入 · Esc 返回\nn 新增 · d 删除 · K/J 移动 · F2 显示", "Enter 編輯 · Ctrl-S 儲存 · Ctrl-O 匯入 · Esc 返回\nn 新增 · d 刪除 · K/J 移動 · F2 顯示"),
    ("仅转换有对应能力的字段；其他字段忽略，未执行节点连通性检查", "Only supported fields are converted; other fields are ignored. Node connectivity has not been tested.", "仅转换支持的字段；忽略其他字段，未检查节点连通性。", "僅轉換支援的欄位；忽略其他欄位，未檢查節點連線。"),
    ("独立代理模式（无需 Rule Core）", "Standalone", "独立模式", "獨立模式"),
    ("独立模式出站标签", "Default outbound", "默认出站", "預設出站"),
    ("规则 Core Socket", "Rule endpoint", "规则端点", "規則端點"),
    ("SOCKS5 监听地址（留空禁用）", "SOCKS5 listener", "SOCKS5 监听", "SOCKS5 監聽"),
    ("HTTP 监听地址（留空禁用）", "HTTP listener", "HTTP 监听", "HTTP 監聽"),
    ("IPC 连接数", "IPC connections", "IPC 连接数", "IPC 連線數"),
    ("规则查询超时（毫秒）", "Rule timeout (ms)", "规则超时（毫秒）", "規則逾時（毫秒）"),
    ("连接超时（秒）", "Connect timeout (s)", "连接超时（秒）", "連線逾時（秒）"),
    ("停止等待时间（秒）", "Shutdown timeout (s)", "停止超时（秒）", "停止逾時（秒）"),
    ("最大连接数", "Max connections", "最大连接数", "最大連線數"),
    ("决策缓存时间（秒，0 禁用）", "Rule cache (s, 0 off)", "规则缓存（秒，0 关闭）", "規則快取（秒，0 關閉）"),
    ("最大 UDP 流数", "Max UDP flows", "最大 UDP 流数", "最大 UDP 流數"),
    ("每个 UDP 关联的目标数", "UDP targets per session", "每会话 UDP 目标数", "每工作階段 UDP 目標數"),
    ("自动监测配置文件并热重载", "Auto reload", "自动重载", "自動重新載入"),
    ("UDP 空闲超时（秒）", "UDP idle timeout (s)", "UDP 空闲超时（秒）", "UDP 閒置逾時（秒）"),
    ("规则监听 Socket", "Rule endpoint", "规则端点", "規則端點"),
    ("GeoSite 数据库", "GeoSite database", "GeoSite 数据库", "GeoSite 資料庫"),
    ("GeoSite 下载地址", "GeoSite download URL", "GeoSite 下载地址", "GeoSite 下載網址"),
    ("GeoSite 更新间隔（秒）", "GeoSite refresh (seconds)", "GeoSite 更新间隔（秒）", "GeoSite 更新間隔（秒）"),
    ("geosite", "GeoSite", "GeoSite", "GeoSite"),
    ("domain_regex", "Domain regex", "域名正则", "網域正規表示式"),
    ("GeoIP 数据库", "GeoIP database", "GeoIP 数据库", "GeoIP 資料庫"),
    ("GeoIP 下载地址", "GeoIP download URL", "GeoIP 下载地址", "GeoIP 下載網址"),
    ("GeoIP 更新间隔（秒）", "GeoIP refresh (seconds)", "GeoIP 更新间隔（秒）", "GeoIP 更新間隔（秒）"),
    ("规则集类型", "Rule-set behavior", "规则集类型", "規則集類型"),
    ("文件格式", "File format", "文件格式", "檔案格式"),
    ("规则集文件或缓存路径", "Rule-set file/cache path", "规则集文件或缓存路径", "規則集檔案或快取路徑"),
    ("规则集下载地址", "Rule-set download URL", "规则集下载地址", "規則集下載網址"),
    ("更新间隔（秒）", "Refresh (seconds)", "更新间隔（秒）", "更新間隔（秒）"),
    ("内联规则列表", "Inline rules", "内联规则列表", "內嵌規則列表"),
    ("规则集", "Rule sets", "规则集", "規則集"),
    ("请输入规则集名称", "Enter a rule-set name", "请输入规则集名称", "請輸入規則集名稱"),
    ("规则集名称已存在", "Rule-set name already exists", "规则集名称已存在", "規則集名稱已存在"),
    ("geoip", "GeoIP", "geoip", "GeoIP"),
    ("rule_set", "Rule set", "rule_set", "規則集"),
    ("domain_keyword", "Domain keyword", "domain_keyword", "網域關鍵字"),
    ("规则集名称：{}▏\nEnter 创建 · Esc 取消", "Rule-set name: {}▏\nEnter create · Esc cancel", "规则集名称：{}▏\nEnter 创建 · Esc 取消", "規則集名稱：{}▏\nEnter 建立 · Esc 取消"),
    ("本地 IP 直连", "Direct local IPs", "本地 IP 直连", "本機 IP 直連"),
    ("process_name", "Process name", "进程名", "處理程序名稱"),
    ("process_path", "Process path", "进程路径", "處理程序路徑"),
    ("默认路由动作", "Default action", "默认动作", "預設動作"),
    ("macOS 网络服务名称列表（空列表自动发现）", "Network services ([] auto)", "网络服务（[] 自动）", "網路服務（[] 自動）"),
    ("TUN 接口名（留空自动分配）", "Adapter name (blank: auto)", "网卡名称（空为自动）", "網卡名稱（空白為自動）"),
    ("TUN IPv4 地址", "IPv4 address", "IPv4 地址", "IPv4 位址"),
    ("TUN 地址前缀", "IPv4 prefix", "IPv4 前缀", "IPv4 前綴"),
    ("TUN IPv6 地址（可选）", "IPv6 address", "IPv6 地址", "IPv6 位址"),
    ("TUN IPv6 地址前缀", "IPv6 prefix", "IPv6 前缀", "IPv6 前綴"),
    ("接管的 CIDR 列表", "Capture routes (CIDR)", "接管路由（CIDR）", "接管路由（CIDR）"),
    ("经原网关绕过的 CIDR 列表（含节点 IP 和 DNS）", "Bypass routes (CIDR)", "绕过路由（CIDR）", "略過路由（CIDR）"),
    ("原出口网关", "Original gateway", "原网关", "原閘道"),
    ("原出口 IPv6 网关（可选）", "Original IPv6 gateway", "原 IPv6 网关", "原 IPv6 閘道"),
    ("原出口接口（如 en0 / eth0 / Ethernet 或 Windows 索引）", "Physical interface", "物理网卡", "實體網卡"),
    ("Egress adapter (physical or virtual)", "Egress adapter (physical or virtual)", "出口网卡（物理或虚拟）", "出口網卡（實體或虛擬）"),
    ("Automatic interface", "Automatic selection", "自动选择", "自動選擇"),
    ("Inherit default interface", "Inherit outer group / default egress", "沿用外层代理组／默认出口", "沿用外層代理組／預設出口"),
    ("Select a local network adapter with ↑↓", "Select a local network adapter with ↑↓", "请用 ↑↓ 选择本机网卡", "請用 ↑↓ 選擇本機網卡"),
    ("Could not list local network adapters", "Could not list local network adapters", "无法读取本机网卡列表", "無法讀取本機網卡清單"),
    ("Linux 透明代理监听地址", "Listener", "监听地址", "監聽位址"),
    ("匹配类型", "Match type", "匹配类型", "比對類型"),
    ("匹配内容", "Match value", "匹配内容", "比對內容"),
    ("路由动作", "Action", "路由动作", "路由動作"),
    ("节点类型", "Node type", "节点类型", "節點類型"),
    ("节点标签", "Node tag", "节点标签", "節點標籤"),
    ("服务器（主机:端口）", "Server (host:port)", "服务器（主机:端口）", "伺服器（主機:連接埠）"),
    ("TLS 服务器名", "TLS server name", "TLS 服务器名", "TLS 伺服器名稱"),
    ("密码", "Password", "密码", "密碼"),
    ("用户名", "Username", "用户名", "使用者名稱"),
    ("代理协议", "Protocol", "协议", "協定"),
    ("服务器主机", "Server host", "服务器主机", "伺服器主機"),
    ("服务器端口", "Server port", "服务器端口", "伺服器連接埠"),
    ("Vision 流控", "Vision flow", "Vision 流控", "Vision 流控"),
    ("VLESS Encryption（完整字符串）", "VLESS encryption", "VLESS 加密", "VLESS 加密"),
    ("加密方式", "Cipher", "加密方式", "加密方式"),
    ("Salamander 混淆密码", "Salamander password", "Salamander 密码", "Salamander 密碼"),
    ("证书 SHA-256 固定值", "Certificate SHA-256 pin", "证书 SHA-256", "憑證 SHA-256"),
    ("UDP 模式", "UDP mode", "UDP 模式", "UDP 模式"),
    ("空闲超时（秒）", "Idle timeout (s)", "空闲超时（秒）", "閒置逾時（秒）"),
    ("拥塞控制", "Congestion control", "拥塞控制", "壅塞控制"),
    ("多路复用协议（可选）", "Multiplexing", "多路复用", "多工"),
    ("自定义 CA 文件", "CA file", "CA 文件", "CA 檔案"),
    ("客户端 TLS 指纹", "Client TLS fingerprint", "客户端 TLS 指纹", "用戶端 TLS 指紋"),
    ("服务端证书 SHA-256 指纹", "Certificate SHA-256 pin", "证书 SHA-256", "憑證 SHA-256"),
    ("REALITY 公钥", "REALITY public key", "REALITY 公钥", "REALITY 公鑰"),
    ("传输方式", "Transport", "传输方式", "傳輸方式"),
    ("请求路径", "Request path", "请求路径", "請求路徑"),
    ("gRPC 服务名", "gRPC service", "gRPC 服务名", "gRPC 服務名稱"),
    ("XHTTP 模式", "XHTTP mode", "XHTTP 模式", "XHTTP 模式"),
    ("系统代理（SOCKS）", "System proxy", "系统代理", "系統代理"),
    ("TUN 接管", "TUN", "TUN", "TUN"),
    ("出站节点", "Outbounds", "出站节点", "出站節點"),
    ("路由规则（按顺序匹配）", "Rules", "路由规则", "路由規則"),
    ("协议参数", "Protocol", "协议", "協定"),
    ("传输参数", "Transport", "传输", "傳輸"),
    ("自定义请求头", "Headers", "请求头", "請求標頭"),
    ("协议", "Protocol", "协议", "協定"),
    ("传输", "Transport", "传输", "傳輸"),
    ("请求头", "Headers", "请求头", "請求標頭"),
    ("direct 直连；block 拦截；proxy:节点标签 使用指定出站", "direct / block / proxy:tag", "direct / block / proxy:标签", "direct / block / proxy:標籤"),
    ("↑↓ 选择匹配类型；更改类型后请检查匹配内容；规则按列表顺序生效", "↑↓ choose; rules match in order", "↑↓ 选择，规则按顺序匹配", "↑↓ 選擇，規則依序比對"),
    ("↑↓ 选择传输方式；切换方式会移除不适用的模式和请求头", "↑↓ choose; switching resets transport options", "↑↓ 选择，切换会重置传输选项", "↑↓ 選擇，切換會重設傳輸選項"),
    ("↑↓ 选择；切换节点类型会重置专用字段并保留标签，保存前检查凭据", "↑↓ choose; switching resets protocol options", "↑↓ 选择，切换会重置协议选项", "↑↓ 選擇，切換會重設協定選項"),
    ("↑↓ 选择；保留可复用参数，重置不适用的协议字段，保存前检查凭据", "↑↓ choose; switching resets protocol options", "↑↓ 选择，切换会重置协议选项", "↑↓ 選擇，切換會重設協定選項"),
    ("true：全部 TCP/UDP 入站使用固定出站，不连接 Rule Core；false：使用规则分流", "Use a fixed outbound instead of rules", "使用固定出站", "使用固定出站"),
    ("填写节点标签；仅有一个节点时可省略，多个节点时必须指定；需启用 standalone", "Required when multiple outbounds are configured", "多个节点时必填", "多個節點時必填"),
    ("输入 1–65535 的整数", "1–65535", "1–65535", "1–65535"),
    ("填写完整 mlkem768x25519plus… 字符串；留空或 none 不启用；不是 cipher", "Full mlkem768x25519plus… value; blank or none to disable", "填写完整加密字符串，空或 none 关闭", "填寫完整加密字串，空白或 none 關閉"),
    ("客户端 ClientHello 样式，例如 chrome；不是服务端证书哈希", "ClientHello profile, e.g. chrome", "ClientHello 样式，如 chrome", "ClientHello 樣式，如 chrome"),
    ("服务端证书 SHA-256：64 位十六进制；替代 CA 验证，不与自定义 CA / REALITY 合用", "64 hex digits; replaces CA verification; incompatible with CA / REALITY", "64 位十六进制，替代 CA 验证；不兼容 CA / REALITY", "64 位十六進位，取代 CA 驗證；不相容 CA / REALITY"),
    ("auto：普通 TLS 选择 packet-up，REALITY 选择 stream-one；省略默认 stream-one", "auto: TLS packet-up, REALITY stream-one", "auto：TLS packet-up，REALITY stream-one", "auto：TLS packet-up，REALITY stream-one"),
    ("无效的条目路径", "Invalid entry path", "条目路径无效", "項目路徑無效"),
    ("条目已不存在，请返回上一级", "Entry no longer exists", "条目不存在，请返回", "項目不存在，請返回"),
    ("该条目不是参数分组", "Not a settings group", "不是参数分组", "不是參數群組"),
    ("没有选择条目列表", "Select a list first", "请先选择列表", "請先選擇清單"),
    ("该分组不是条目列表", "Not a list", "不是条目列表", "不是項目清單"),
    ("配置格式无法解析。原草稿已保留，请修复配置后重新打开", "Invalid configuration; draft retained", "配置格式无效，草稿已保留", "設定格式無效，草稿已保留"),
    ("列表中需要参数条目", "Expected a settings entry", "列表需要参数条目", "清單需要參數項目"),
    ("请先确认或取消当前条目窗口，再保存配置", "Confirm or cancel this entry before saving", "请先确认或取消当前编辑", "請先確認或取消目前編輯"),
    ("字段已不存在", "Field no longer exists", "字段不存在", "欄位不存在"),
    ("请输入整数", "Enter an integer", "请输入整数", "請輸入整數"),
    ("{} 必须在 {}–{} 之间", "{} must be between {} and {}", "{} 须在 {}–{} 之间", "{} 須在 {}–{} 之間"),
    ("请用 ↑↓ 选择支持的值：{}", "Choose with ↑↓: {}", "↑↓ 选择：{}", "↑↓ 選擇：{}"),
    ("请输入 TOML 字符串列表，例如 [\"en0\", \"eth0\"]", "Enter a string list, e.g. [\"a\", \"b\"]", "请输入字符串列表，如 [\"a\", \"b\"]", "請輸入字串清單，如 [\"a\", \"b\"]"),
    ("必须是字符串列表", "Expected a string list", "须为字符串列表", "須為字串清單"),
    ("列表中的值必须是字符串", "List values must be strings", "列表值须为字符串", "清單值須為字串"),
    ("请选择 true 或 false", "Choose true or false", "请选择 true 或 false", "請選擇 true 或 false"),
    ("{} 不能为空", "{} is required", "{} 不能为空", "{} 不能為空"),
    ("TLS 参数路径无效", "Invalid TLS settings path", "TLS 参数路径无效", "TLS 參數路徑無效"),
    ("请输入有效的 HTTP 请求头名称", "Enter a valid HTTP header name", "请输入有效的请求头名称", "請輸入有效的標頭名稱"),
    ("该请求头已存在", "Header already exists", "请求头已存在", "標頭已存在"),
    ("必填项不能删除，请按 Enter 修改", "Required field; Enter to edit", "必填项，Enter 编辑", "必填項，Enter 編輯"),
    ("请先选择要调整顺序的规则或节点", "Select a rule or node first", "请先选择规则或节点", "請先選擇規則或節點"),
    ("条目编辑 · {}{} · 单页 / 仅节点折叠", "Edit · {}{}", "编辑 · {}{}", "編輯 · {}{}"),
    ("配置格式无法解析，草稿已保留。请修复配置文件后重新打开。Esc 返回。", "Invalid configuration; draft retained. Esc to return.", "配置格式无效，草稿已保留。Esc 返回。", "設定格式無效，草稿已保留。Esc 返回。"),
    ("[TLS 未启用；可直接填写或在 TLS 行启用]", "[TLS off]", "[TLS 关闭]", "[TLS 關閉]"),
    ("[必填，尚未设置；建议 {}]", "[required: {}]", "[必填：{}]", "[必填：{}]"),
    ("输入凭据", "credentials", "凭据", "認證資訊"),
    ("[未设置{}]", "[unset{}]", "[未设置{}]", "[未設定{}]"),
    ("，默认 {}", ", default {}", "，默认 {}", "，預設 {}"),
    (" · 已启用 / Enter 关闭", " · on", " · 开启", " · 開啟"),
    (" · 未启用 / Enter 启用", " · off", " · 关闭", " · 關閉"),
    ("  ＋ 新增请求头（Enter）", "  + Header", "  + 请求头", "  + 請求標頭"),
    ("Enter 展开/收起节点；展开后全部参数平铺；d 删除；K/J 调整顺序", "Enter expand / collapse · d delete · K/J move", "Enter 展开/收起 · d 删除 · K/J 排序", "Enter 展開/收合 · d 刪除 · K/J 排序"),
    ("规则字段已在下方平铺；d 删除整条规则；K/J 上移/下移", "d delete · K/J move", "d 删除 · K/J 排序", "d 刪除 · K/J 排序"),
    ("Enter 启用/关闭 TLS；字段始终平铺；d 删除本分组参数", "Enter toggle TLS · d remove", "Enter 开关 TLS · d 删除", "Enter 開關 TLS · d 刪除"),
    ("Enter 新增请求头；已有请求头直接在上方修改", "Enter add header", "Enter 新增请求头", "Enter 新增標頭"),
    ("全部参数在同一页；↑↓ 选择，Enter 编辑，n 新增，Ctrl-S 校验保存", "↑↓ select · Enter edit · Ctrl-S save", "↑↓ 选择 · Enter 编辑 · Ctrl-S 保存", "↑↓ 選擇 · Enter 編輯 · Ctrl-S 儲存"),
    ("(未设置)", "(unset)", "(未设置)", "(未設定)"),
    ("新增出站节点 · ↑↓ 选择 · Enter 确认 · Esc 取消", "New outbound · Enter select · Esc cancel", "新增节点 · Enter 确认 · Esc 取消", "新增節點 · Enter 確認 · Esc 取消"),
    ("新增请求头", "New header", "新增请求头", "新增標頭"),
    ("删除所选条目/分组？\n只修改草稿，Ctrl-S 后写入配置。\ny 确认删除 · n / Esc 取消", "Delete selection?\ny delete · Esc cancel", "删除所选条目？\ny 删除 · Esc 取消", "刪除所選項目？\ny 刪除 · Esc 取消"),
    ("确认删除", "Delete?", "确认删除", "確認刪除"),
    ("连接中", "Connecting", "连接中", "連線中"),
    ("实时连接", "Connected", "已连接", "已連線"),
    ("已停止订阅；r 重连", "Disconnected · r reconnect", "已断开 · r 重连", "已中斷 · r 重新連線"),
    ("等待实例产生日志（订阅保持连接）", "Waiting for logs", "等待日志", "等待日誌"),
    ("── 日志已轮换或截断，继续接收 ──", "── Log rotated ──", "── 日志已轮换 ──", "── 日誌已輪替 ──"),
    (" [超长行已截断]", " [line truncated]", " [已截断]", " [已截斷]"),
    ("{} [超长行已截断]", "{} [line truncated]", "{} [已截断]", "{} [已截斷]"),
    ("跟随", "Follow", "跟随", "跟隨"),
    ("暂停跟随", "Paused", "暂停", "暫停"),
    ("全部", "All", "全部", "全部"),
    ("过滤：仅显示含关键词的行", "Filter", "过滤", "篩選"),
    ("搜索：高亮关键词，n/N 跳转", "Search · n/N next / previous", "搜索 · n/N 跳转", "搜尋 · n/N 跳轉"),
    ("全部（包含无级别日志）", "All", "全部", "全部"),
    ("{} 及以上", "{} and above", "{} 及以上", "{} 及以上"),
    ("显示级别 · ↑↓ 选择 · Enter 应用", "Display level · Enter apply", "显示级别 · Enter 应用", "顯示級別 · Enter 套用"),
    ("core 记录级别 · 即时生效 · Enter 应用", "Capture level · Enter apply", "记录级别 · Enter 应用", "記錄級別 · Enter 套用"),
    ("记录级别已改为 {}，无需重启", "Log level: {}", "记录级别：{}", "記錄級別：{}"),
    ("节点 {} · {} [{}]", "Node {} · {} [{}]", "节点 {} · {} [{}]", "節點 {} · {} [{}]"),
    ("规则 {} · {} {} → {}", "Rule {} · {} {} → {}", "规则 {} · {} {} → {}", "規則 {} · {} {} → {}"),
    ("规则 {}", "Rule {}", "规则 {}", "規則 {}"),
    ("── {}（{} 条） · n 新增 ──", "── {} ({}) · n add ──", "── {}（{}） · n 新增 ──", "── {}（{}） · n 新增 ──"),
    ("> {}\n{}\nEnter 确认 · Esc 取消 · ↑↓ 选择 · Ctrl-U 清空\nF2 显示/隐藏凭据；输入会替换选中值\n{}", "> {}\n{}\nEnter apply · Esc cancel · Ctrl-U clear · F2 reveal\n{}", "> {}\n{}\nEnter 确认 · Esc 取消 · Ctrl-U 清空 · F2 显示\n{}", "> {}\n{}\nEnter 確認 · Esc 取消 · Ctrl-U 清除 · F2 顯示\n{}"),
    ("请求头名称：{}▏\nEnter 继续设置值 · Esc 取消", "Header: {}▏\nEnter continue · Esc cancel", "请求头：{}▏\nEnter 继续 · Esc 取消", "標頭：{}▏\nEnter 繼續 · Esc 取消"),
    ("实时日志 · {} · {} · 显示 {} · 记录 {}", "Logs · {} · {} · filter {} · capture {}", "日志 · {} · {} · 显示 {} · 记录 {}", "日誌 · {} · {} · 顯示 {} · 記錄 {}"),
    ("{} · 显示 {}/{} 行 · 缓冲淘汰 {} 行\n过滤: {}  搜索: {}", "{} · {}/{} lines · {} dropped\nFilter: {}  Search: {}", "{} · {}/{} 行 · 淘汰 {} 行\n过滤：{}  搜索：{}", "{} · {}/{} 行 · 淘汰 {} 行\n篩選：{}  搜尋：{}"),
    ("> {}▏\nEnter 应用 · Esc 取消 · Ctrl-U 清空", "> {}▏\nEnter apply · Esc cancel · Ctrl-U clear", "> {}▏\nEnter 应用 · Esc 取消 · Ctrl-U 清空", "> {}▏\nEnter 套用 · Esc 取消 · Ctrl-U 清除"),
    ("Proxy", "Proxy", "代理", "代理"),
    ("Rules", "Rules", "规则", "規則"),
    ("Paused", "Paused", "操作暂停", "操作暫停"),
    ("Instances", "Instances", "实例", "執行個體"),
    ("n to add an instance", "n to add an instance", "n 新增实例", "n 新增執行個體"),
    (" · reload pending", " · reload pending", " · 待重载", " · 待重新載入"),
    ("Running", "Running", "运行中", "執行中"),
    ("Stopped", "Stopped", "已停止", "已停止"),
    ("Started", "Started", "已启动", "已啟動"),
    ("Stopped (drain timeout; forced)", "Stopped (timeout)", "已停止（超时）", "已停止（逾時）"),
    ("Stopped (IPC unavailable; forced)", "Stopped (forced)", "已强制停止", "已強制停止"),
    ("Restarted with current configuration", "Restarted", "已重启", "已重新啟動"),
    ("Configuration valid", "Configuration valid", "配置有效", "設定有效"),
    ("Instance added", "Instance added", "已新增实例", "已新增執行個體"),
    ("Instance removed; configuration retained", "Instance removed; configuration kept", "实例已移除，配置保留", "執行個體已移除，設定保留"),
    ("Configuration copied; use c to select it", "Configuration copied", "配置已复制", "設定已複製"),
    ("Validated and saved; hot applied when running; previous file in .toml.bak", "Saved", "已保存", "已儲存"),
    ("Manager and all instances stopped", "All stopped", "全部已停止", "全部已停止"),
    ("Name (1–24: a-z, 0-9, -_)", "Name (1–24: a-z, 0-9, -_)", "名称（1–24 位字母、数字、-_）", "名稱（1–24 位字母、數字、-_）"),
    ("Configuration path", "Configuration path", "配置路径", "設定路徑"),
    ("Copy to path", "Copy to path", "复制到路径", "複製到路徑"),
    ("Remove instance? Configuration is kept.", "Remove instance? Configuration is kept.", "移除实例？配置将保留。", "移除執行個體？設定將保留。"),
    ("Stop all instances?", "Stop all instances?", "停止全部实例？", "停止全部執行個體？"),
    ("Discard changes?", "Discard changes?", "放弃修改？", "捨棄變更？"),
    ("Review configuration · Ctrl-S save", "Review configuration · Ctrl-S save", "检查配置 · Ctrl-S 保存", "檢查設定 · Ctrl-S 儲存"),
    ("Ctrl-S save", "Ctrl-S save", "Ctrl-S 保存", "Ctrl-S 儲存"),
    ("Working…", "Working…", "处理中…", "處理中…"),
    ("Cancelled", "Cancelled", "已取消", "已取消"),
    ("Edit the value · Enter confirm", "Edit the value · Enter confirm", "修改内容 · Enter 确认", "修改內容 · Enter 確認"),
    ("Retrying…", "Retrying…", "重试中…", "重試中…"),
    ("c retry · b cancel · ↑↓ scroll", "c retry · b cancel · ↑↓ scroll", "c 重试 · b 取消 · ↑↓ 滚动", "c 重試 · b 取消 · ↑↓ 捲動"),
    ("Enter confirm · Esc cancel", "Enter confirm · Esc cancel", "Enter 确认 · Esc 取消", "Enter 確認 · Esc 取消"),
    ("/ search · f filter · l level · Space pause · Esc back\nn/N next/previous · L capture · r reconnect", "/ search · f filter · l level · Space pause · Esc back\nn/N next/previous · L capture · r reconnect", "/ 搜索 · f 过滤 · l 显示级别 · 空格 暂停 · Esc 返回\nn/N 跳转 · L 记录级别 · r 重连", "/ 搜尋 · f 篩選 · l 顯示級別 · 空白 暫停 · Esc 返回\nn/N 跳轉 · L 記錄級別 · r 重新連線"),
    ("Enter edit · Ctrl-S save · Esc back · n add · d delete\nK/J move · F2 reveal", "Enter edit · Ctrl-S save · Esc back · n add · d delete\nK/J move · F2 reveal", "Enter 编辑 · Ctrl-S 保存 · Esc 返回 · n 新增 · d 删除\nK/J 排序 · F2 显示凭据", "Enter 編輯 · Ctrl-S 儲存 · Esc 返回 · n 新增 · d 刪除\nK/J 排序 · F2 顯示認證資訊"),
    ("s start · x stop · r restart · e edit · l logs · n add\nc config · R reload · v check · y copy · d remove · q quit · Q stop all", "s start · x stop · r restart · e edit · l logs · n add\nc config · R reload · v check · d remove · q quit · Q stop all", "s 启动 · x 停止 · r 重启 · e 编辑 · l 日志 · n 新增\nc 配置 · R 重载 · v 检查 · d 移除 · q 退出 · Q 停止全部", "s 啟動 · x 停止 · r 重新啟動 · e 編輯 · l 日誌 · n 新增\nc 設定 · R 重新載入 · v 檢查 · d 移除 · q 退出 · Q 停止全部"),
    ("Shortcuts", "Shortcuts", "快捷键", "快速鍵"),
    ("F1 help · Esc back", "F1 help · Esc back", "F1 帮助 · Esc 返回", "F1 說明 · Esc 返回"),
    ("Log subscription disconnected; continue to reconnect", "Log disconnected; c to reconnect", "日志已断开，c 重连", "日誌已中斷，c 重新連線"),
    ("--manager: run manager · --shutdown: stop all\nq: close UI; instances keep running", "--manager: run manager · --shutdown: stop all\nq: close UI; instances keep running", "--manager：启动管理服务 · --shutdown：停止全部\nq：退出界面，实例继续运行", "--manager：啟動管理服務 · --shutdown：停止全部\nq：退出介面，執行個體繼續執行"),
    ("Unresponsive", "Unresponsive", "无响应", "無回應"),
    ("Operation ended; draft retained in memory; session saving disabled until next launch", "Cancelled; draft kept in memory, session saving unavailable", "已取消；草稿仅保留在内存中，无法保存会话", "已取消；草稿僅保留於記憶體，無法儲存工作階段"),
    ("Operation ended; editor draft retained", "Cancelled; draft retained", "已取消，草稿已保留", "已取消，草稿已保留"),
    ("Draft retained; correct the file or permissions, then Ctrl-S to retry", "Draft retained · Ctrl-S retry", "草稿已保留 · Ctrl-S 重试", "草稿已保留 · Ctrl-S 重試"),
    ("Configuration hot reloaded; existing sessions retained", "Reloaded", "已重载", "已重新載入"),
    ("Configuration selected and hot applied when running", "Configuration selected", "已选择配置", "已選擇設定"),
    ("Cannot save session: {}", "Cannot save session: {}", "无法保存会话：{}", "無法儲存工作階段：{}"),
    ("Cannot restore session: {}", "Cannot restore session: {}", "无法恢复会话：{}", "無法還原工作階段：{}"),
    ("Name: 1–24 letters, digits, - or _", "Name: 1–24 letters, digits, - or _", "名称须为 1–24 位字母、数字、- 或 _", "名稱須為 1–24 位字母、數字、- 或 _"),
    ("Enter a configuration file path", "Enter a configuration file path", "请输入配置路径", "請輸入設定路徑"),
    ("Log disconnected: {}", "Log disconnected: {}", "日志已断开：{}", "日誌已中斷：{}"),
    ("Start", "Start", "启动", "啟動"),
    ("Stop", "Stop", "停止", "停止"),
    ("Restart", "Restart", "重启", "重新啟動"),
    ("Reload", "Reload", "重载配置", "重新載入設定"),
    ("Edit", "Edit configuration", "编辑配置", "編輯設定"),
    ("Logs", "Logs", "日志", "日誌"),
    ("Startup bindings", "Startup bindings", "绑定启动", "繫結啟動"),
    ("Copy configuration", "Copy configuration", "复制配置", "複製設定"),
    ("Remove", "Remove instance", "移除实例", "移除執行個體"),
    ("Restart manager to enable startup bindings", "Restart manager to enable startup bindings", "重启管理服务后可设置绑定启动", "重新啟動管理服務後可設定繫結啟動"),
    ("Create a rule instance first", "Create a rule instance first", "请先创建规则实例", "請先建立規則執行個體"),
    ("Create a proxy instance first", "Create a proxy instance first", "请先创建代理实例", "請先建立代理執行個體"),
    ("Not running", "Not running", "实例尚未运行", "執行個體尚未執行"),
    ("Stop instance before removing", "Stop instance before removing", "请先停止实例", "請先停止執行個體"),
    ("Startup bindings saved", "Startup bindings saved", "启动绑定已保存", "啟動繫結已儲存"),
    ("Standalone", "Standalone", "独立模式", "獨立模式"),
    ("System proxy", "System proxy", "系统代理", "系統代理"),
    ("Configuration unavailable", "Configuration unavailable", "配置不可用", "設定無法使用"),
    ("No listeners", "No listeners", "未设置监听", "未設定監聽"),
    ("Instances ({})", "Instances ({})", "实例（{}）", "執行個體（{}）"),
    ("{} rules", "{} rules", "{} 条规则", "{} 條規則"),
    ("{} outbounds: {}", "{} outbounds: {}", "{} 个出站：{}", "{} 個出站：{}"),
    ("Uptime", "Uptime", "运行", "執行"),
    ("Log level", "Log", "日志", "日誌"),
    ("Enter actions · s start · x stop · e edit · l logs\nb bindings · n add · Tab switch · q quit · Q stop all", "Enter actions · s start · x stop · e edit · l logs\nb bindings · n add · Tab switch · q quit · Q stop all", "Enter 操作 · s 启动 · x 停止 · e 编辑 · l 日志\nb 绑定启动 · n 新增 · Tab 切换 · q 退出 · Q 停止全部", "Enter 操作 · s 啟動 · x 停止 · e 編輯 · l 日誌\nb 繫結啟動 · n 新增 · Tab 切換 · q 退出 · Q 停止全部"),
    ("Enter actions · s start · x stop · e edit · l logs\nn add · Tab switch · q quit · Q stop all", "Enter actions · s start · x stop · e edit · l logs\nn add · Tab switch · q quit · Q stop all", "Enter 操作 · s 启动 · x 停止 · e 编辑 · l 日志\nn 新增 · Tab 切换 · q 退出 · Q 停止全部", "Enter 操作 · s 啟動 · x 停止 · e 編輯 · l 日誌\nn 新增 · Tab 切換 · q 退出 · Q 停止全部"),
    ("Startup bindings · {}", "Startup bindings · {}", "绑定启动 · {}", "繫結啟動 · {}"),
    ("Actions · {}", "Actions · {}", "操作 · {}", "操作 · {}"),
    ("Space select · Enter save · Esc cancel", "Space select · Enter save · Esc cancel", "空格 选择 · Enter 保存 · Esc 取消", "空白 選擇 · Enter 儲存 · Esc 取消"),
    ("↑↓ select · Enter apply · Esc back", "↑↓ select · Enter apply · Esc back", "↑↓ 选择 · Enter 确认 · Esc 返回", "↑↓ 選擇 · Enter 確認 · Esc 返回"),
    ("Enter confirm · Esc cancel · Ctrl-U clear", "Enter confirm · Esc cancel · Ctrl-U clear", "Enter 确认 · Esc 取消 · Ctrl-U 清空", "Enter 確認 · Esc 取消 · Ctrl-U 清除"),
    ("Already running", "Already running", "实例已在运行", "執行個體已在執行"),
    ("Already stopped", "Already stopped", "实例已停止", "執行個體已停止"),
    ("No outbounds", "No outbounds", "未配置出站", "未設定出站"),
    ("Nothing to undo", "Nothing to undo", "没有可撤销的修改", "沒有可復原的修改"),
    ("Nothing to redo", "Nothing to redo", "没有可重做的修改", "沒有可重做的修改"),
    ("Undone", "Undone · Ctrl-Y redo", "已撤销 · Ctrl-Y 重做", "已復原 · Ctrl-Y 重做"),
    ("Redone", "Redone", "已重做", "已重做"),
    ("Deleted · u / Ctrl-Z undo", "Deleted · u / Ctrl-Z undo", "已删除 · u / Ctrl-Z 撤销", "已刪除 · u / Ctrl-Z 復原"),
    ("Duplicated · u / Ctrl-Z undo", "Duplicated · u / Ctrl-Z undo", "已复制 · u / Ctrl-Z 撤销", "已複製 · u / Ctrl-Z 復原"),
    ("Select a node or rule to duplicate", "Select a node or rule to duplicate", "请先选择要复制的节点或规则", "請先選取要複製的節點或規則"),
    ("No match for \"{}\"", "No match for \"{}\"", "没有找到“{}”", "找不到「{}」"),
    ("All template values replaced · Ctrl-S save", "All template values replaced · Ctrl-S save", "示例值已全部填写 · Ctrl-S 保存", "範例值已全部填寫 · Ctrl-S 儲存"),
    ("Filling new entry · Enter next field · Esc stop", "Filling new entry · Enter confirms and opens the next field · Esc stop", "正在填写新条目 · Enter 确认并进入下一项 · Esc 停止", "正在填寫新項目 · Enter 確認並進入下一項 · Esc 停止"),
    ("Enter edit/fold · n new group · r rename · d delete", "Enter edit/fold · n new group · r rename · d delete", "Enter 编辑/展开 · n 新建组 · r 重命名 · d 删除", "Enter 編輯/展開 · n 新增群組 · r 重新命名 · d 刪除"),
    ("Enter / ←→ fold · n add · zM/zR fold/open all", "Enter / ←→ fold · n add · zM/zR fold/open all", "Enter / ←→ 展开收起 · n 新增 · zM/zR 全部收起/展开", "Enter / ←→ 展開收合 · n 新增 · zM/zR 全部收合/展開"),
    ("Enter / Space toggle · d reset to default", "Enter / Space toggle · d reset to default", "Enter / 空格 切换 · d 恢复默认", "Enter / 空白鍵 切換 · d 恢復預設"),
    ("Enter edit · required", "Enter edit · required", "Enter 编辑 · 必填", "Enter 編輯 · 必填"),
    ("Enter edit · d reset to default", "Enter edit · d reset to default", "Enter 编辑 · d 恢复默认", "Enter 編輯 · d 恢復預設"),
    ("Enter fold · y duplicate · d delete · K/J move", "Enter fold · y duplicate · d delete · K/J move", "Enter 展开/收起 · y 复制 · d 删除 · K/J 移动", "Enter 展開/收合 · y 複製 · d 刪除 · K/J 移動"),
    ("y duplicate · d delete · K/J move", "y duplicate · d delete · K/J move", "y 复制 · d 删除 · K/J 移动", "y 複製 · d 刪除 · K/J 移動"),
    ("Enter edit · n add", "Enter edit · n add", "Enter 编辑 · n 新增", "Enter 編輯 · n 新增"),
    ("unsaved", "unsaved", "未保存", "未儲存"),
    ("TLS off · filling any field enables it", "TLS off · filling any field enables it", "TLS 未启用 · 填写任一项即启用", "TLS 未啟用 · 填寫任一項即啟用"),
    ("default", "default", "默认", "預設"),
    ("template value", "template value, replace it", "示例值，请替换", "範例值，請替換"),
    ("required", "required", "必填", "必填"),
    ("  {} · n add", "  {} · n add", "  {} 条 · n 新增", "  {} 筆 · n 新增"),
    (" · rules {}–{}", " · rules {}–{}", " · 规则 {}–{}", " · 規則 {}–{}"),
    ("Enter jump · Esc cancel", "Enter jump to next match · Esc cancel", "Enter 跳到下一个匹配 · Esc 取消", "Enter 跳到下一個符合項目 · Esc 取消"),
    ("/ search · u undo · Ctrl-Y redo · Ctrl-S save · ? all keys · Esc back", "/ search · u undo · Ctrl-Y redo · Ctrl-S save · ? all keys · Esc back", "/ 搜索 · u 撤销 · Ctrl-Y 重做 · Ctrl-S 保存 · ? 全部按键 · Esc 返回", "/ 搜尋 · u 復原 · Ctrl-Y 重做 · Ctrl-S 儲存 · ? 全部按鍵 · Esc 返回"),
    ("+ Add subscription (Enter)", "+ Add subscription (Enter)", "＋ 添加订阅（Enter）", "＋ 新增訂閱（Enter）"),
];

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn regional_locales_and_fallback() {
        for locale in ["zh-Hant", "zh_TW.UTF-8", "zh-HK", "zh-MO"] {
            assert_eq!(Language::resolve(locale), Language::Traditional);
        }
        for locale in ["zh-CN", "zh_Hans", "zh"] {
            assert_eq!(Language::resolve(locale), Language::Simplified);
        }
        for locale in ["en-US", "C", "de_DE.UTF-8"] {
            assert_eq!(Language::resolve(locale), Language::English);
        }
    }
    #[test]
    fn translations_keep_all_placeholders() {
        let mut keys = std::collections::HashSet::new();
        for (source, english, simplified, traditional) in CATALOG {
            assert!(keys.insert(source), "Duplicate translation: {source}");
            let count = source.matches("{}").count();
            for translation in [english, simplified, traditional] {
                assert_eq!(translation.matches("{}").count(), count, "{source}");
            }
        }
    }
}
