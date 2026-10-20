use crate::i18n::{language, Language};

fn all_text() -> &'static str {
    match language() {
        Language::English => "General\n? / Esc: close this help · ↑↓ / j k: scroll · PgUp/PgDn: page\nCtrl-C: leave TUI, retain running cores and draft\n\nInstances\nTab / h l / ←→: switch Proxy/Rules · ↑↓ / j k: select\ngg / Home: first · G / End: last · Ctrl-U/D: page\nEnter: action menu · n: add · e: edit · L: logs\ns: start · x: stop · r: restart/start · R: reload\nb: startup bindings · c: configuration path · y: clone config · d: remove stopped instance\nv: validate config · t: proxy connectivity / rule query · q: quit · Q: stop all (confirmation)\nAction menu: all applicable operations, including subscription update\n\nConfiguration\n↑↓ / j k: select · Enter: edit/toggle · h l / ←→: collapse/expand\ngg / G: first/last · Ctrl-U/D: page · zM/zR: fold/open categories\nn: add node/rule/set/group/subscription · d: delete · K/J: reorder\nr: rename set/group · e: toggle rule set · F2: reveal secret\nCtrl-O: import file/share links · F4: external editor · F5: read external draft\nCtrl-S: validate/save · Ctrl-T: rule query · Esc: return\nFields: type/paste · Tab: complete · ↑↓: suggestions · Enter: apply · Esc: cancel · Ctrl-U: clear\nMember picker: ↑↓ select · Space toggle · Enter apply · Esc cancel\n\nLogs\n↑↓ / j k: scroll · ←→ / h l: horizontal (wrap off)\nPgUp/PgDn / Ctrl-U/D: page · gg/Home: first · G/End: follow\nSpace: pause/follow · w: auto wrap on/off\n/: search · n/N: next/previous match · f: filter · Backspace: clear filters\nv: display level · L: capture level · m: memory line limit\nr: reconnect · Esc: back\n\nDialogs and diagnostics\nEnter: apply · Esc: cancel/back · Ctrl-U: clear text\n↑↓ / j k: select/scroll · Space: toggle binding/member · t: repeat connectivity test\nRecovery: c retry · b cancel · ↑↓ scroll\nDestructive operations require the confirmation shown in the dialog.",
        Language::Simplified => "通用\n? / Esc 关闭帮助 · ↑↓ / j k 滚动 · PgUp/PgDn 翻页\nCtrl-C 退出界面，保留运行中的核心和草稿\n\n实例列表\nTab / h l / ←→ 切换代理与规则 · ↑↓ / j k 选择\ngg / Home 首项 · G / End 末项 · Ctrl-U/D 翻页\nEnter 操作菜单 · n 添加 · e 编辑 · L 日志\ns 启动 · x 停止 · r 重启/启动 · R 热重载\nb 启动绑定 · c 配置路径 · y 复制配置 · d 删除已停止实例\nv 校验配置 · t 代理连通性测试/规则查询 · q 退出 · Q 停止全部（需确认）\n操作菜单列出适用的全部操作，包括立即更新订阅\n\n配置编辑\n↑↓ / j k 选择 · Enter 编辑/切换 · h l / ←→ 收起/展开\ngg / G 首末项 · Ctrl-U/D 翻页 · zM/zR 收起/展开类别\nn 添加节点/规则/规则集/组/订阅 · d 删除 · K/J 调整顺序\nr 重命名规则集/代理组 · e 启停规则集 · F2 显示敏感字段\nCtrl-O 导入文件/分享链接 · F4 外部编辑器 · F5 重读外部草稿\nCtrl-S 校验保存 · Ctrl-T 规则查询 · Esc 返回\n字段中输入/粘贴 · Tab 补全 · ↑↓ 选择建议 · Enter 确认 · Esc 取消 · Ctrl-U 清空\n成员选择：↑↓ 选择 · 空格勾选 · Enter 应用 · Esc 取消\n\n日志\n↑↓ / j k 滚动 · ←→ / h l 横向滚动（关闭换行时）\nPgUp/PgDn / Ctrl-U/D 翻页 · gg/Home 首行 · G/End 跟随末尾\n空格 暂停/跟随 · w 开关自动换行\n/ 搜索 · n/N 下个/上个匹配 · f 过滤 · Backspace 清除过滤\nv 显示级别 · L 核心记录级别 · m 内存保留条数\nr 重连 · Esc 返回\n\n对话框与诊断\nEnter 确认 · Esc 取消/返回 · Ctrl-U 清空文本\n↑↓ / j k 选择或滚动 · 空格 勾选绑定/成员 · t 重新测试连通性\n恢复界面：c 重试 · b 取消 · ↑↓ 滚动\n删除及停止全部等操作按对话框显示的内容确认。",
        Language::Traditional => "通用\n? / Esc 關閉說明 · ↑↓ / j k 捲動 · PgUp/PgDn 翻頁\nCtrl-C 離開介面，保留執行中的核心與草稿\n\n實例列表\nTab / h l / ←→ 切換代理與規則 · ↑↓ / j k 選擇\ngg / Home 首項 · G / End 末項 · Ctrl-U/D 翻頁\nEnter 操作選單 · n 新增 · e 編輯 · L 日誌\ns 啟動 · x 停止 · r 重新啟動 · R 熱重載\nb 啟動綁定 · c 設定路徑 · y 複製設定 · d 刪除已停止實例\nv 驗證設定 · t 代理連線測試/規則查詢 · q 離開 · Q 停止全部（須確認）\n操作選單列出適用的全部操作，包括立即更新訂閱\n\n設定編輯\n↑↓ / j k 選擇 · Enter 編輯/切換 · h l / ←→ 收合/展開\ngg / G 首末項 · Ctrl-U/D 翻頁 · zM/zR 收合/展開類別\nn 新增節點/規則/規則集/群組/訂閱 · d 刪除 · K/J 調整順序\nr 重新命名規則集/代理群組 · e 啟停規則集 · F2 顯示敏感欄位\nCtrl-O 匯入檔案/分享連結 · F4 外部編輯器 · F5 重讀外部草稿\nCtrl-S 驗證儲存 · Ctrl-T 規則查詢 · Esc 返回\n欄位中輸入/貼上 · Tab 補全 · ↑↓ 選擇建議 · Enter 確認 · Esc 取消 · Ctrl-U 清空\n成員選擇：↑↓ 選擇 · 空白鍵勾選 · Enter 套用 · Esc 取消\n\n日誌\n↑↓ / j k 捲動 · ←→ / h l 橫向捲動（關閉換行時）\nPgUp/PgDn / Ctrl-U/D 翻頁 · gg/Home 首行 · G/End 跟隨末尾\n空白鍵 暫停/跟隨 · w 開關自動換行\n/ 搜尋 · n/N 下個/上個符合項目 · f 篩選 · Backspace 清除篩選\nv 顯示級別 · L 核心記錄級別 · m 記憶體保留筆數\nr 重新連線 · Esc 返回\n\n對話框與診斷\nEnter 確認 · Esc 取消/返回 · Ctrl-U 清空文字\n↑↓ / j k 選擇或捲動 · 空白鍵 勾選綁定/成員 · t 重新測試連線\n復原介面：c 重試 · b 取消 · ↑↓ 捲動\n刪除與停止全部等操作按對話框顯示的內容確認。",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Instances,
    ProxyEditor,
    RuleEditor,
    Subscriptions,
    Logs,
    Diagnostics,
    Route,
    Bindings,
    Menu,
    Recovery,
}
pub fn text(page: Page) -> String {
    let section = match page {
        Page::Instances => Some(1),

        Page::Logs => Some(3),
        _ => None,
    };
    if let Some(section) = section {
        let text = all_text()
            .split("\n\n")
            .nth(section)
            .unwrap_or("")
            .to_owned();
        return text;
    }
    let texts = match page {
        Page::ProxyEditor => ["Proxy configuration\n↑↓ / j k select · Enter edit/toggle · h l fold/open\ngg/G ends · Ctrl-U/D page · zM/zR fold/open groups\nn add node/group/subscription · d delete · K/J reorder\nr rename proxy group · F2 reveal secret\nCtrl-O import · F4 external editor · F5 read draft\nCtrl-S validate/save · Esc back", "代理配置\n↑↓ / j k 选择 · Enter 编辑/切换 · h l 收起/展开\ngg/G 首末项 · Ctrl-U/D 翻页 · zM/zR 收起/展开组\nn 添加节点/组/订阅 · d 删除 · K/J 调整顺序\nr 重命名代理组 · F2 显示敏感字段\nCtrl-O 导入 · F4 外部编辑 · F5 重读草稿\nCtrl-S 校验保存 · Esc 返回", "代理設定\n↑↓ / j k 選擇 · Enter 編輯/切換 · h l 收合/展開\ngg/G 首末項 · Ctrl-U/D 翻頁 · zM/zR 收合/展開群組\nn 新增節點/群組/訂閱 · d 刪除 · K/J 調整順序\nr 重新命名代理群組 · F2 顯示敏感欄位\nCtrl-O 匯入 · F4 外部編輯 · F5 重讀草稿\nCtrl-S 驗證儲存 · Esc 返回"],
        Page::RuleEditor => ["Rule configuration\n↑↓ / j k select · Enter edit/toggle · h l fold/open\ngg/G ends · Ctrl-U/D page · zM/zR fold/open categories\nn add rule/set · d delete · K/J reorder\nr rename rule set · e enable/disable set\nCtrl-T query route · Ctrl-O import\nF4 external editor · F5 read draft · Ctrl-S save · Esc back", "规则配置\n↑↓ / j k 选择 · Enter 编辑/切换 · h l 收起/展开\ngg/G 首末项 · Ctrl-U/D 翻页 · zM/zR 收起/展开类别\nn 添加规则/规则集 · d 删除 · K/J 调整顺序\nr 重命名规则集 · e 启停规则集\nCtrl-T 规则查询 · Ctrl-O 导入\nF4 外部编辑 · F5 重读草稿 · Ctrl-S 保存 · Esc 返回", "規則設定\n↑↓ / j k 選擇 · Enter 編輯/切換 · h l 收合/展開\ngg/G 首末項 · Ctrl-U/D 翻頁 · zM/zR 收合/展開類別\nn 新增規則/規則集 · d 刪除 · K/J 調整順序\nr 重新命名規則集 · e 啟停規則集\nCtrl-T 規則查詢 · Ctrl-O 匯入\nF4 外部編輯 · F5 重讀草稿 · Ctrl-S 儲存 · Esc 返回"],
        Page::Subscriptions => ["Subscriptions\nn / Enter on Add: add source · Enter: edit field\nd: remove source · Ctrl-S: save and trigger update\nNodes below each source are read-only; refresh automatically\nCtrl-O: import · F4: external editor · F5: read draft · Esc: back", "订阅列表\nn / 添加项 Enter：新增订阅 · Enter 编辑字段\nd 删除订阅 · Ctrl-S 保存并触发更新\n每个订阅下方显示所属节点，只读并自动刷新\nCtrl-O 导入 · F4 外部编辑 · F5 重读草稿 · Esc 返回", "訂閱列表\nn / 新增項 Enter：新增訂閱 · Enter 編輯欄位\nd 刪除訂閱 · Ctrl-S 儲存並觸發更新\n每個訂閱下方顯示所屬節點，唯讀並自動更新\nCtrl-O 匯入 · F4 外部編輯 · F5 重讀草稿 · Esc 返回"],
        Page::Diagnostics => ["Connectivity / Exit IP\n↑↓ / j k: scroll · PgUp/PgDn: page\nt: repeat test · Esc: back", "连通性 / 出口 IP\n↑↓ / j k 滚动 · PgUp/PgDn 翻页\nt 重新测试 · Esc 返回", "連線 / 出口 IP\n↑↓ / j k 捲動 · PgUp/PgDn 翻頁\nt 重新測試 · Esc 返回"],
        Page::Route => ["Route explanation\n↑↓ / j k: scroll · PgUp/PgDn: page · Esc: back", "命中解释\n↑↓ / j k 滚动 · PgUp/PgDn 翻页 · Esc 返回", "命中說明\n↑↓ / j k 捲動 · PgUp/PgDn 翻頁 · Esc 返回"],
        Page::Bindings => ["Startup bindings\n↑↓ / j k: select · Space: toggle\nEnter: apply · Esc: cancel", "启动绑定\n↑↓ / j k 选择 · 空格勾选\nEnter 应用 · Esc 取消", "啟動綁定\n↑↓ / j k 選擇 · 空白鍵勾選\nEnter 套用 · Esc 取消"],
        Page::Recovery => ["Recovery\nc: retry · b: cancel · ↑↓: scroll · Ctrl-C: leave TUI", "错误恢复\nc 重试 · b 取消 · ↑↓ 滚动 · Ctrl-C 离开界面", "錯誤復原\nc 重試 · b 取消 · ↑↓ 捲動 · Ctrl-C 離開介面"],
        _ => ["Action menu\n↑↓ / j k: select · Enter: execute · Esc: close", "操作菜单\n↑↓ / j k 选择 · Enter 执行 · Esc 关闭", "操作選單\n↑↓ / j k 選擇 · Enter 執行 · Esc 關閉"],
    };
    texts[match language() {
        Language::English => 0,
        Language::Simplified => 1,
        Language::Traditional => 2,
    }]
    .into()
}

pub fn popup(area: ratatui::layout::Rect, text: &str) -> ratatui::layout::Rect {
    use ratatui::widgets::{Paragraph, Wrap};
    let preferred = (ratatui::text::Text::from(text).width() + 4).clamp(28, 76) as u16;
    let width = area
        .width
        .saturating_sub(4)
        .min(preferred)
        .max(1)
        .min(area.width);
    let height = (Paragraph::new(text)
        .wrap(Wrap { trim: false })
        .line_count(width.saturating_sub(2).max(1)) as u16
        + 2)
    .min(19)
    .min(area.height.saturating_sub(2).max(1))
    .min(area.height);
    ratatui::layout::Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn help_is_page_specific_and_popups_fit_content_and_small_terminals() {
        crate::i18n::with_language(Language::English, || {
            let logs = text(Page::Logs);
            assert!(
                logs.contains("w:")
                    && !logs.contains("Ctrl-O")
                    && !logs.contains("startup bindings")
            );
            let subscriptions = text(Page::Subscriptions);
            assert!(
                subscriptions.contains("read-only") && !subscriptions.contains("capture level")
            );
            let rule = text(Page::RuleEditor);
            assert!(rule.contains("Ctrl-T") && !rule.contains("add node"));
            let proxy = text(Page::ProxyEditor);
            assert!(!proxy.contains("Ctrl-T") && !proxy.contains("rule set"));
            let area = ratatui::layout::Rect::new(0, 0, 200, 60);
            let popup = popup(area, &text(Page::Diagnostics));
            assert!(popup.width < 76 && popup.height < 12 && popup.x > 0 && popup.y > 0);
            for (w, h) in [(10, 5), (30, 12), (80, 24)] {
                let area = ratatui::layout::Rect::new(0, 0, w, h);
                let p = super::popup(area, &logs);
                assert!(p.right() <= w && p.bottom() <= h);
            }
        });
    }
}
