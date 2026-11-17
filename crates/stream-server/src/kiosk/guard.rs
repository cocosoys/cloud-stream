//! 键位判定：锁定期间，哪些按键不该到达系统。
//!
//! ## 为什么判定要单独成一层
//!
//! 同一个判据有**两个**执行点，而它们拿到的键码根本不是一回事：
//!
//! * 注入端（[`crate::dispatch::win_input`]）拿到的是浏览器上报的 X11 keysym；
//! * 键盘钩子（[`super::hook`]）拿到的是 Windows 的虚拟键码 `VK_*`。
//!
//! 如果两处各写一遍规则，它们必然会走偏 —— 一处拦住了 `Alt+Tab`、另一处漏掉
//! 了 `Alt+Esc`，而这种偏差只在"玩家真的按了那个键"时才暴露。所以规则只在
//! 这里写一次：先把两套键码都翻译成同一个 [`Chord`]，再用同一个 [`judge`] 判。
//!
//! ## 判据的边界
//!
//! 目标是"别切出去"，不是"屏蔽键盘"。所以：
//!
//! * `F11` **必须放行** —— 它就是 MC 的全屏键，而这个项目的串流链路实测能采到
//!   独占全屏（见 `game/mod.rs` 开头）。拦掉它等于拦掉玩家唯一想要的全屏。
//! * `Tab`、`Esc`、`Alt`、`Ctrl` 单独按下**必须放行** —— 它们都是游戏内正常
//!   按键（Tab 在 MC 里是玩家列表）。
//! * `Win` 键本身要吞。它一按下，开始菜单就盖住整个桌面，而串流采的是整块屏幕。
//!
//! ## 一个自洽的小性质
//!
//! 被吞下的键仍然记进 `held`。于是"Win 按住 → 后续所有键都吞"不需要第二个状态
//! 位：`Win` 在 `held` 里，[`Mods::win`] 就一直是真的。松开时从 `held` 移走，
//! 压制自然解除。状态只有 `held` 与 `swallowed` 两个，且都能被单测直接观察。

/// X11 keysym：`Shift_L`（左 Shift）。
pub const XK_SHIFT_L: u32 = 0xffe1;
/// X11 keysym：`Shift_R`。
pub const XK_SHIFT_R: u32 = 0xffe2;
/// X11 keysym：`Control_L`。
pub const XK_CONTROL_L: u32 = 0xffe3;
/// X11 keysym：`Control_R`。
pub const XK_CONTROL_R: u32 = 0xffe4;
/// X11 keysym：`Meta_L`（在 Windows 上映射到左 Win）。
pub const XK_META_L: u32 = 0xffe7;
/// X11 keysym：`Meta_R`（右 Win）。
pub const XK_META_R: u32 = 0xffe8;
/// X11 keysym：`Alt_L`。
pub const XK_ALT_L: u32 = 0xffe9;
/// X11 keysym：`Alt_R`（AltGr）。
pub const XK_ALT_R: u32 = 0xffea;
/// X11 keysym：`Super_L`（左 Win）。
pub const XK_SUPER_L: u32 = 0xffeb;
/// X11 keysym：`Super_R`（右 Win）。
pub const XK_SUPER_R: u32 = 0xffec;
/// X11 keysym：`Hyper_L`。
pub const XK_HYPER_L: u32 = 0xffed;
/// X11 keysym：`Hyper_R`。
pub const XK_HYPER_R: u32 = 0xffee;

/// X11 keysym：`Tab`。
pub const XK_TAB: u32 = 0xff09;
/// X11 keysym：`Return`。
pub const XK_RETURN: u32 = 0xff0d;
/// X11 keysym：`Escape`。
pub const XK_ESCAPE: u32 = 0xff1b;
/// X11 keysym：`space`。
pub const XK_SPACE: u32 = 0x20;
/// X11 keysym：`F4`。`0xffbe` 是 `F1`，逐位递增。
pub const XK_F4: u32 = 0xffbe + 3;

/// Windows 虚拟键：左 Win。
pub const VK_LWIN: u16 = 0x5b;
/// Windows 虚拟键：右 Win。
pub const VK_RWIN: u16 = 0x5c;
/// Windows 虚拟键：`Tab`。
pub const VK_TAB: u16 = 0x09;
/// Windows 虚拟键：`Escape`。
pub const VK_ESCAPE: u16 = 0x1b;
/// Windows 虚拟键：`F4`。
pub const VK_F4: u16 = 0x73;
/// Windows 虚拟键：`space`。
pub const VK_SPACE: u16 = 0x20;
/// Windows 虚拟键：`Return`。
pub const VK_RETURN: u16 = 0x0d;

/// 一次"危险组合"的归类。两套键码都翻译到这个枚举，规则于是只有一份。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chord {
    /// 不是组合键，按普通键处理。
    Plain,
    /// `Win` 键本身（左/右）。
    Win,
    /// `Alt+Tab`。
    AltTab,
    /// `Alt+Esc`。
    AltEsc,
    /// `Alt+F4`。
    AltF4,
    /// `Alt+Space`（系统菜单）。
    AltSpace,
    /// `Alt+Enter`。
    AltEnter,
    /// `Ctrl+Esc`（开始菜单）。
    CtrlEsc,
}

/// 当前按下的修饰键集合。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Mods {
    pub alt: bool,
    pub ctrl: bool,
    pub shift: bool,
    pub win: bool,
}

/// 这套判定唯一的规则表。`true` = 吞掉，不交给系统。
///
/// `mods.win` 对所有键都返回 `true` 是有意的：Win 被按下之后，系统的下一次按键
/// 就会被解释成 Win 组合（`Win+X`、`Win+R`……），而它们全部会离开游戏。既然
/// Win 本身已经在 `held` 里，这里省掉一个"污染中"状态位。
pub fn judge(mods: Mods, chord: Chord) -> bool {
    match chord {
        Chord::Win => true,
        Chord::AltTab | Chord::AltEsc | Chord::AltF4 | Chord::AltSpace | Chord::AltEnter => {
            mods.alt || mods.win
        }
        // `Ctrl+Shift+Esc` 打开任务管理器、`Ctrl+Esc` 打开开始菜单 —— 前者会把
        // 一个全屏窗口压在游戏上面，后者直接换掉整个桌面。
        Chord::CtrlEsc => mods.ctrl || mods.win,
        Chord::Plain => mods.win,
    }
}

/// `Esc` 在注入端也要认 `Ctrl`：`Ctrl+Esc` 与 `Alt+Esc` 都要吞，但只有后者
/// 能由 [`Chord`] 单独表达。
fn judge_keysym(mods: Mods, keysym: u32) -> bool {
    if keysym == XK_ESCAPE && mods.ctrl {
        return true;
    }
    judge(mods, chord_of_keysym(keysym))
}

/// 把 X11 keysym 归类。
pub fn chord_of_keysym(keysym: u32) -> Chord {
    match keysym {
        XK_SUPER_L | XK_SUPER_R | XK_META_L | XK_META_R | XK_HYPER_L | XK_HYPER_R => Chord::Win,
        XK_TAB => Chord::AltTab,
        XK_ESCAPE => Chord::AltEsc,
        XK_F4 => Chord::AltF4,
        XK_SPACE => Chord::AltSpace,
        XK_RETURN => Chord::AltEnter,
        _ => Chord::Plain,
    }
}

/// 把 Windows 虚拟键归类。
///
/// 注意 `Esc` 在这里同时可能是 `Ctrl+Esc` 或 `Alt+Esc`：归类只描述"这个键参与
/// 哪种组合"，最终吞不吞仍由 [`judge`] 结合修饰键决定。
pub fn chord_of_vk(vk: u16) -> Chord {
    match vk {
        VK_LWIN | VK_RWIN => Chord::Win,
        VK_TAB => Chord::AltTab,
        VK_ESCAPE => Chord::AltEsc,
        VK_F4 => Chord::AltF4,
        VK_SPACE => Chord::AltSpace,
        VK_RETURN => Chord::AltEnter,
        _ => Chord::Plain,
    }
}

/// `Esc` 的两种组合在这里合流：`Alt+Esc` 与 `Ctrl+Esc` 是一回事（都要吞）。
fn judge_vk(mods: Mods, vk: u16) -> bool {
    if vk == VK_ESCAPE && mods.ctrl {
        return true;
    }
    judge(mods, chord_of_vk(vk))
}

fn mods_of(held: &[u32]) -> Mods {
    Mods {
        alt: held.iter().any(|&k| k == XK_ALT_L || k == XK_ALT_R),
        ctrl: held.iter().any(|&k| k == XK_CONTROL_L || k == XK_CONTROL_R),
        shift: held.iter().any(|&k| k == XK_SHIFT_L || k == XK_SHIFT_R),
        win: held
            .iter()
            .any(|&k| matches!(k, XK_SUPER_L | XK_SUPER_R | XK_META_L | XK_META_R | XK_HYPER_L | XK_HYPER_R)),
    }
}

/// 注入端的有状态滤波器。
///
/// 状态有两个：
///
/// * `held` —— 逻辑上按下的键（**包括被吞掉的**）。只靠注入成功的键算不出来，
///   因为被吞的 `Win` 永远不会进入系统。
/// * `swallowed` —— 按下去时被吞的键，松开时要吞掉对应的 key-up。少了它，游戏
///   会收到一串"没按下过就抬起"的事件；MC 的 LWJGL 对无配对的 up 一般无视，但
///   那属于"看起来没事"而不是"对的"。
#[derive(Debug, Default)]
pub struct KeyGuard {
    armed: bool,
    held: Vec<u32>,
    swallowed: Vec<u32>,
}

impl KeyGuard {
    /// 开关。切换时清空状态：把上一个状态里的 `held` 带过来，会让解锁后第一次
    /// 按键被莫名吞掉（或者按住中的键永远松不开）。
    pub fn set_armed(&mut self, armed: bool) {
        if self.armed != armed {
            self.held.clear();
            self.swallowed.clear();
        }
        self.armed = armed;
    }

    pub fn is_armed(&self) -> bool {
        self.armed
    }

    /// `true` = 交给注入端真的按下去。
    pub fn on_down(&mut self, keysym: u32) -> bool {
        if !self.armed {
            return true;
        }
        if self.held.contains(&keysym) {
            // 重复的 down（浏览器重发、`keys_sync` 补发）不再注入，否则 Windows
            // 侧会累积按键计数，松开一次不够。
            return false;
        }
        self.held.push(keysym);
        if judge_keysym(mods_of(&self.held), keysym) {
            self.swallowed.push(keysym);
            return false;
        }
        true
    }

    /// `true` = 交给注入端真的抬起来。
    pub fn on_up(&mut self, keysym: u32) -> bool {
        if !self.armed {
            return true;
        }
        self.held.retain(|&k| k != keysym);
        match self.swallowed.iter().position(|&k| k == keysym) {
            Some(idx) => {
                self.swallowed.remove(idx);
                false
            }
            None => true,
        }
    }

    /// 断开连接 / 重置时的兜底：状态清空，交给调用方去释放注入侧真的按着的键。
    pub fn release_all(&mut self) {
        self.held.clear();
        self.swallowed.clear();
    }

    /// 供日志与测试观察。
    pub fn held(&self) -> &[u32] {
        &self.held
    }
}

/// 键盘钩子用的无状态判定（钩子自己维护 Win 的按下状态）。
///
/// 与 [`KeyGuard::on_down`] 分开，是因为钩子拿到的是 `VK` 与"此刻的物理修饰键
/// 状态"，而注入端拿到的是有序的 keysym 流。两者共享的只有 [`judge`]。
pub fn hook_blocks(vk: u16, mods: Mods, win_down: bool) -> bool {
    let mods = Mods {
        win: mods.win || win_down,
        ..mods
    };
    judge_vk(mods, vk)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> KeyGuard {
        let mut g = KeyGuard::default();
        g.set_armed(true);
        g
    }

    #[test]
    fn win_key_itself_is_swallowed() {
        let mut g = guard();
        assert!(!g.on_down(XK_SUPER_L));
        assert_eq!(g.held(), &[XK_SUPER_L]);
    }

    #[test]
    fn everything_is_swallowed_while_win_is_held() {
        let mut g = guard();
        assert!(!g.on_down(XK_SUPER_L));
        // Win+X 的 X、Win+R 的 R —— 它们本身是普通键，但此刻属于 Win 组合。
        assert!(!g.on_down(b'x' as u32));
        assert!(!g.on_up(b'x' as u32), "被吞的键，抬起也不能注入");
        assert!(!g.on_down(b'r' as u32));
        assert!(!g.on_up(b'r' as u32));
        // 松开 Win 之后恢复。
        assert!(!g.on_up(XK_SUPER_L));
        assert!(g.on_down(b'x' as u32));
    }

    #[test]
    fn alt_tab_is_swallowed_but_lone_tab_is_not() {
        let mut g = guard();
        assert!(g.on_down(XK_TAB), "单独按 Tab 是玩家列表，必须放行");
        assert!(g.on_up(XK_TAB));

        assert!(g.on_down(XK_ALT_L), "Alt 本身要放行，游戏里也用它");
        assert!(!g.on_down(XK_TAB), "Alt 按住时的 Tab 是切窗口");
        assert!(!g.on_up(XK_TAB), "被吞的键，抬起也不能注入");
        assert!(g.on_up(XK_ALT_L));

        // Alt 松开后 Tab 重新可注入。
        assert!(g.on_down(XK_TAB));
    }

    #[test]
    fn alt_f4_and_alt_esc_and_alt_space_are_swallowed() {
        for key in [XK_F4, XK_ESCAPE, XK_SPACE, XK_RETURN] {
            let mut g = guard();
            assert!(g.on_down(XK_ALT_L));
            assert!(!g.on_down(key), "Alt+0x{key:x} 必须吞掉");
        }
    }

    #[test]
    fn ctrl_esc_and_ctrl_shift_esc_are_swallowed() {
        let mut g = guard();
        assert!(g.on_down(XK_CONTROL_L));
        assert!(!g.on_down(XK_ESCAPE));

        let mut g = guard();
        assert!(g.on_down(XK_CONTROL_L));
        assert!(g.on_down(XK_SHIFT_L));
        assert!(!g.on_down(XK_ESCAPE), "Ctrl+Shift+Esc 是任务管理器");
    }

    #[test]
    fn f11_stays_available() {
        // 这是整个锁定的前提：F11 是 MC 的全屏键，串流能采到独占全屏。
        let mut g = guard();
        assert!(g.on_down(0xffbe + 10), "F11 必须放行");
        assert!(g.on_up(0xffbe + 10));
        // 带修饰键也一样（MC 里没人用 Alt+F11，但不该被拦）。
        assert!(g.on_down(XK_CONTROL_L));
        assert!(g.on_down(0xffbe + 10));
        assert!(g.on_up(0xffbe + 10));
        assert!(g.on_up(XK_CONTROL_L));
    }

    #[test]
    fn disarmed_guard_passes_everything() {
        let mut g = KeyGuard::default();
        assert!(g.on_down(XK_SUPER_L));
        assert!(g.on_down(XK_TAB));
        assert!(g.on_up(XK_SUPER_L));
    }

    #[test]
    fn toggling_armed_clears_state() {
        let mut g = guard();
        assert!(!g.on_down(XK_SUPER_L));
        g.set_armed(false);
        g.set_armed(true);
        // 上一轮被吞的 Win 不能留下痕迹：Tab 现在必须放行。
        assert!(g.on_down(XK_TAB));
        assert!(g.on_up(XK_SUPER_L), "旧状态清空后，抬起没有对应记录");
    }

    #[test]
    fn duplicate_down_is_not_reinjected() {
        let mut g = guard();
        assert!(g.on_down(b'a' as u32));
        assert!(!g.on_down(b'a' as u32), "重复 down 不注入，避免按键计数累积");
    }

    #[test]
    fn hook_blocks_win_and_chords() {
        let none = Mods::default();
        assert!(hook_blocks(VK_LWIN, none, false));
        assert!(hook_blocks(VK_RWIN, none, false));
        assert!(hook_blocks(VK_TAB, Mods { alt: true, ..none }, false));
        assert!(hook_blocks(VK_ESCAPE, Mods { ctrl: true, ..none }, false));
        assert!(hook_blocks(VK_F4, Mods { alt: true, ..none }, false));
        // 单独按的普通键、F11、Shift+Tab 都要放行。
        assert!(!hook_blocks(VK_TAB, none, false));
        assert!(!hook_blocks(0x7a, none, false), "F11 = 0x7a");
        assert!(!hook_blocks(VK_TAB, Mods { shift: true, ..none }, false));
    }

    #[test]
    fn hook_blocks_everything_while_win_down() {
        // Win 的按下被钩子吞了，系统层看不到它，所以钩子自己持有这个状态。
        assert!(hook_blocks(b'R' as u16, Mods::default(), true));
        assert!(!hook_blocks(b'R' as u16, Mods::default(), false));
    }
}
