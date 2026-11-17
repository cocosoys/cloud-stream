//! 版本 json 的参数模板：占位符、`rules`、以及"后者覆盖前者"的合并。
//!
//! 这一层是照 PCL2 的 `ModLaunch.vb` 抄的，理由不是"它有"，而是它解决了三个必须
//! 解决的问题：
//!
//! 1. **参数不该写在 Rust 里**。旧实现把 1.12.2 那二十几个 token 逐条 `push`，
//!    等于把版本 json 里已经有一份的模板又抄了一遍。换版本要改代码，改错了没有
//!    对照物 —— `javaw` 只会回一句 `Could not find or load main class`。
//! 2. **占位符要一次替干净**。`${auth_player_name}`、`${game_directory}` 这些
//!    名字是 Mojang 定的协议，不是我们的口味问题。留下一个没替换的，java 会把它
//!    当成一个普通字符串传给 MC，症状是"用户名变成了 `${auth_player_name}`"。
//! 3. **重复的键必须有一个确定的胜者**。`--width` 在模板里和追加段里各出现一次
//!    时，MC 只认解析到的第一个 —— 于是"改了窗口大小却没变"成了最难查的一类
//!    故障。PCL2 的办法是拼接后统一去重（新值覆盖旧值），这样"改尺寸"永远不用
//!    动模板。这里抄的就是这一条。
//!
//! 与 PCL2 的差别只有一处：它把 JVM 参数也整体模板化，我们保留自己的 JVM 底座
//! （内存、GC、`-XX:ErrorFile`），只把 json 的 `arguments.jvm` 作为补充 —— 那些
//! `-Dos.name`、`-Djava.library.path` 由我们算得更准（见 [`super::plan`]）。

use std::collections::BTreeMap;

use serde_json::Value;

use super::ArgToken;

// ------------------------------------------------------------------ 占位符

/// 展开模板时能用的全部变量。
///
/// 键是协议里的名字（`${` 与 `}` 之间那一段），值是本地事实。没有用 `HashMap`
/// 是因为它只在这一处创建、只读几百次，有序反而让 `--print-game` 的输出稳定。
#[derive(Debug, Default, Clone)]
pub struct Placeholders {
    map: BTreeMap<String, String>,
}

impl Placeholders {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, key: &str, value: impl Into<String>) -> &mut Self {
        self.map.insert(key.to_owned(), value.into());
        self
    }

    /// 把 `text` 里的 `${name}` 全部换掉。
    ///
    /// 遇到不认识的占位符直接报错而不是留个原样：留下来的那一串会被 java 当成
    /// 普通参数值交给 MC，故障表现与"哪个变量缺失"完全无关。宁可在启动前失败。
    pub fn expand(&self, text: &str) -> Result<String, String> {
        if !text.contains("${") {
            return Ok(text.to_owned());
        }
        let mut out = String::with_capacity(text.len() + 32);
        let mut rest = text;
        while let Some(i) = rest.find("${") {
            out.push_str(&rest[..i]);
            let after = &rest[i + 2..];
            // 没有闭合的 `}`：不是占位符，原样留着（启动前校验会点出来）。
            let Some(j) = after.find('}') else {
                out.push_str(&rest[i..]);
                return Ok(out);
            };
            let name = &after[..j];
            match self.map.get(name) {
                Some(v) => out.push_str(v),
                None => {
                    return Err(format!(
                        "参数模板里的占位符 ${{{name}}} 没有对应的值（已提供 {} 个：{}）",
                        self.map.len(),
                        short_keys(&self.map)
                    ))
                }
            }
            rest = &after[j + 1..];
        }
        out.push_str(rest);
        Ok(out)
    }
}

fn short_keys(map: &BTreeMap<String, String>) -> String {
    let keys: Vec<&str> = map.keys().map(|s| s.as_str()).take(6).collect();
    if map.len() > 6 {
        format!("{} 等", keys.join("、"))
    } else {
        keys.join("、")
    }
}

// --------------------------------------------------------------------- rules

/// `rules` 里能问到的"环境事实"。只有三项：这是给 Minecraft 用的启动器，不是
/// 通用条件引擎，多一个开关就多一处需要解释的默认值。
#[derive(Debug, Clone, Copy, Default)]
pub struct Features {
    pub demo_user: bool,
    pub custom_resolution: bool,
    pub quick_plays_support: bool,
}

impl Features {
    fn lookup(self, key: &str) -> bool {
        match key {
            "is_demo_user" => self.demo_user,
            "has_custom_resolution" => self.custom_resolution,
            "has_quick_plays_support" => self.quick_plays_support,
            // 不认识的开关一律 false：vanilla 的语义是"要求为 true 才命中"，
            // 我们给不出 true 的理由。
            _ => false,
        }
    }
}

/// 我们只跑 Windows / x86_64。写成常量是为了让下面两处判据不会各写一个拼写错误。
const OS_NAME: &str = "windows";
const OS_ARCH: &str = "x86_64";

fn rule_matches(rule: &Value, feat: Features) -> bool {
    if let Some(os) = rule.get("os") {
        if let Some(name) = os.get("name").and_then(Value::as_str) {
            if !name.eq_ignore_ascii_case(OS_NAME) {
                return false;
            }
        }
        if let Some(arch) = os.get("arch").and_then(Value::as_str) {
            if !arch.eq_ignore_ascii_case(OS_ARCH) {
                return false;
            }
        }
        // `os.version` 是正则（`^10\.` 之类）。没有正则引擎，按"匹配"处理：
        // 判成不匹配会静默少一个库，判成匹配最多是多一个用不上的库。
    }
    if let Some(features) = rule.get("features").and_then(Value::as_object) {
        for (key, want) in features {
            let want = want.as_bool().unwrap_or(false);
            if feat.lookup(key) != want {
                return false;
            }
        }
    }
    true
}

/// `rules` 数组的裁决：空 / 缺失 → 放行；否则**最后一条命中的规则**说了算，
/// 一条都没命中 → 不放行。
///
/// 也接受"单个规则对象"而不是数组：1.12.2 的 `lwjgl-platform:2.9.2` 就写成了
/// `"rules": {"action": "allow", "os": {"name": "osx"}}`（少一对方括号）。把它当成
/// "没有规则"会得到相反的结论 —— 那条规则的意图恰恰是"只在 osx 上装"。
///
/// 库的 `rules` 与参数的 `rules` 是同一种结构，所以同一个函数给两边用 —— 旧实现
/// 给库单独写了一份只认 `os.name`/`os.arch` 的判断，两份迟早会分叉。
pub fn rules_allow(rules: Option<&Value>, feat: Features) -> bool {
    let arr: &[Value] = match rules {
        Some(Value::Array(list)) => list,
        Some(single @ Value::Object(_)) => std::slice::from_ref(single),
        _ => return true,
    };
    if arr.is_empty() {
        return true;
    }
    let mut allow = false;
    for rule in arr {
        if rule_matches(rule, feat) {
            allow = rule
                .get("action")
                .and_then(Value::as_str)
                .map(|a| a.eq_ignore_ascii_case("allow"))
                .unwrap_or(false);
        }
    }
    allow
}

// -------------------------------------------------------------- 参数收集

/// `minecraftArguments`（1.12 及以前的旧式一整串）→ token。
///
/// 先按引号感知切分再逐段替换占位符，顺序不能反：`--gameDir "${game_directory}"`
/// 这类写法里，引号是模板的一部分，先替换再切分会把值里的空格当分隔符。
pub fn args_from_string(raw: &str, ph: &Placeholders) -> Result<Vec<ArgToken>, String> {
    let mut out = Vec::with_capacity(32);
    for token in split_arguments(raw) {
        out.push(ArgToken::json(ph.expand(&token)?));
    }
    Ok(out)
}

/// `arguments.game` / `arguments.jvm`（1.13+ 的新式数组）→ token。
///
/// 元素有两种形态：纯字符串，以及 `{ "rules": [...], "value": "x" | ["x","y"] }`。
/// 后者的 `rules` 不通过就整条跳过 —— 1.13+ 的 `--width/--height` 正是靠
/// `has_custom_resolution` 这一条决定要不要传。
pub fn args_from_array(
    arr: &Value,
    ph: &Placeholders,
    feat: Features,
) -> Result<Vec<ArgToken>, String> {
    let mut out = Vec::new();
    let Some(items) = arr.as_array() else { return Ok(out) };
    for item in items {
        if let Some(text) = item.as_str() {
            out.push(ArgToken::json(ph.expand(text)?));
            continue;
        }
        if !rules_allow(item.get("rules"), feat) {
            continue;
        }
        match item.get("value") {
            Some(Value::String(text)) => out.push(ArgToken::json(ph.expand(text)?)),
            Some(Value::Array(list)) => {
                for v in list {
                    if let Some(text) = v.as_str() {
                        out.push(ArgToken::json(ph.expand(text)?));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

/// 按空白切分参数串，**尊重双引号**。
///
/// 版本 json 的 `minecraftArguments` 是一整串文本：`split_whitespace` 会把
/// `--gameDir "C:\Program Files\x"` 切成三段。PCL 为此写了 `SplitJavaArguments`
/// 状态机，这里只做它最小的一件事 —— 引号内外切换、引号内的 `\"` 还原。
pub fn split_arguments(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut touched = false;
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if in_quotes && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
                touched = true;
            }
            '"' => {
                in_quotes = !in_quotes;
                touched = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if touched {
                    out.push(std::mem::take(&mut cur));
                    touched = false;
                }
            }
            c => {
                cur.push(c);
                touched = true;
            }
        }
    }
    if touched {
        out.push(cur);
    }
    out
}

/// `["--tweakClass", "x.Foo", ...]` 里取 `x.Foo`。
pub fn find_flag_value(tokens: &[String], flag: &str) -> Option<String> {
    tokens
        .iter()
        .position(|t| t.eq_ignore_ascii_case(flag))
        .and_then(|i| tokens.get(i + 1))
        .cloned()
}

// -------------------------------------------------------------- 合并与去重

/// `--width` → `--width`（小写）；不是 `--` 开头（或只有 `--`）→ `None`。
///
/// 只认 `--`：`-Dfoo=1` 这类 JVM 属性重复是合法的（后者覆盖前者），把它们纳入
/// 去重会改掉 java 自己的语义。
fn flag_key(token: &str) -> Option<String> {
    if token.starts_with("--") && token.len() > 2 {
        Some(token.to_ascii_lowercase())
    } else {
        None
    }
}

/// 一个 flag 后面跟的是不是它的值。
///
/// 判据是"下一个 token 自己不是 `--flag`" —— 这是启发式，但所有真实模板里
/// `--key value` 都是成对出现的，而 `--demo` 这种无值开关后面跟的一定是下一个键。
fn has_value(tokens: &[ArgToken], i: usize) -> bool {
    tokens.get(i + 1).map(|n| flag_key(&n.text).is_none()).unwrap_or(false)
}

/// 后者覆盖前者：同一个 `--flag` 只保留**最后一次**出现的那一对。
///
/// 返回被覆盖掉的键名，供 `--print-game` 显示"哪几个参数被谁顶掉了"。这个列表
/// 本身就是一份诊断信息：`--width` 出现在里面，说明"模板里的尺寸被实例设置盖住
/// 了"，这正是期望行为；而 `--username` 出现在里面通常意味着有人重复追加。
pub fn override_flags(tokens: Vec<ArgToken>) -> (Vec<ArgToken>, Vec<String>) {
    let mut last: BTreeMap<String, usize> = BTreeMap::new();
    for (i, t) in tokens.iter().enumerate() {
        if let Some(k) = flag_key(&t.text) {
            last.insert(k, i);
        }
    }

    let mut out: Vec<ArgToken> = Vec::with_capacity(tokens.len());
    let mut overridden: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < tokens.len() {
        let Some(key) = flag_key(&tokens[i].text) else {
            out.push(tokens[i].clone());
            i += 1;
            continue;
        };
        let with_value = has_value(&tokens, i);
        if last.get(&key).copied() == Some(i) {
            out.push(tokens[i].clone());
            if with_value {
                out.push(tokens[i + 1].clone());
            }
        } else if !overridden.contains(&key) {
            overridden.push(key);
        }
        i += if with_value { 2 } else { 1 };
    }
    (out, overridden)
}

/// 值展开成空串的 `--flag ""` 连键一起删掉。
///
/// PCL2 对 `${version_type}` 为空时就是这么做的：`--versionType ""` 会让 MC 把
/// 版本类型显示成空，而正确的做法是"这个版本没有类型"。
pub fn drop_empty_values(tokens: Vec<ArgToken>) -> Vec<ArgToken> {
    let mut out = Vec::with_capacity(tokens.len());
    let mut i = 0usize;
    while i < tokens.len() {
        let is_flag = flag_key(&tokens[i].text).is_some();
        if is_flag && tokens.get(i + 1).map(|n| n.text.is_empty()).unwrap_or(false) {
            i += 2;
            continue;
        }
        out.push(tokens[i].clone());
        i += 1;
    }
    out
}

/// 段内重复的 `--flag`（去重之后仍然存在的那些）。
///
/// 去重本身会把重复消掉，所以这个函数在正常路径上恒为空 —— 它是断言，不是策略：
/// 非空就说明 [`override_flags`] 有漏（比如某个键被拆成了两组 token）。
pub fn duplicate_flags(tokens: &[ArgToken]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    let mut dup: Vec<String> = Vec::new();
    for t in tokens {
        let Some(key) = flag_key(&t.text) else { continue };
        if seen.iter().any(|x| x.eq_ignore_ascii_case(&key)) {
            if !dup.iter().any(|d| d.eq_ignore_ascii_case(&key)) {
                dup.push(key);
            }
        } else {
            seen.push(key);
        }
    }
    dup
}

// -------------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;

    fn ph() -> Placeholders {
        let mut p = Placeholders::new();
        p.set("auth_player_name", "CloudPlayer");
        p.set("game_directory", r"C:\g\saves\default");
        p.set("version_type", "");
        p
    }

    #[test]
    fn expand_replaces_known_and_rejects_unknown() {
        let p = ph();
        assert_eq!(p.expand("${auth_player_name}").unwrap(), "CloudPlayer");
        assert_eq!(p.expand("--gameDir ${game_directory}").unwrap(), r"--gameDir C:\g\saves\default");
        // 没有占位符时原样返回，连内存都不该多分配一份逻辑。
        assert_eq!(p.expand("plain").unwrap(), "plain");
        // 未知占位符必须报错，报错里要有名字 —— 否则排查等于从头猜。
        let err = p.expand("--x ${nope}").unwrap_err();
        assert!(err.contains("${nope}"), "{err}");
        // 没有闭合的 `}` 不算占位符，原样留着交给启动前校验。
        assert_eq!(p.expand("--x ${unclosed").unwrap(), "--x ${unclosed");
    }

    #[test]
    fn rules_follow_last_matching_action() {
        let feat = Features { custom_resolution: true, ..Default::default() };
        // 缺失 / 空数组都放行。
        assert!(rules_allow(None, feat));
        let empty = serde_json::json!([]);
        assert!(rules_allow(Some(&empty), feat));

        // 1.13+ 的真实写法：只有一条 allow，条件不满足就落回默认的 disallow。
        let custom = serde_json::json!([
            {"action": "allow", "features": {"has_custom_resolution": true}}
        ]);
        assert!(rules_allow(Some(&custom), feat), "custom_resolution 为真时命中");
        assert!(
            !rules_allow(Some(&custom), Features::default()),
            "条件不满足且没有其它规则 → 不放行"
        );

        // 两条规则时**最后命中的那条**说了算，不是"有一条 allow 就放行"。
        let two = serde_json::json!([
            {"action": "allow", "os": {"name": "windows"}},
            {"action": "disallow", "features": {"is_demo_user": true}}
        ]);
        assert!(rules_allow(Some(&two), feat), "第二条不命中，保留第一条的 allow");
        assert!(
            !rules_allow(Some(&two), Features { demo_user: true, ..Default::default() }),
            "第二条命中 disallow，覆盖前面的 allow"
        );

        // os 判据：非 windows 一律不命中，一条都没命中 → 不放行。
        let only_linux = serde_json::json!([{"action": "allow", "os": {"name": "linux"}}]);
        assert!(!rules_allow(Some(&only_linux), feat));
        let win = serde_json::json!([{"action": "allow", "os": {"name": "windows", "arch": "x86_64"}}]);
        assert!(rules_allow(Some(&win), feat));
        let x86 = serde_json::json!([{"action": "allow", "os": {"name": "windows", "arch": "x86"}}]);
        assert!(!rules_allow(Some(&x86), feat), "32 位规则在 x86_64 上不命中");

        // 少一对方括号的写法（1.12.2 的 lwjgl-platform:2.9.2 就是这样）：
        // 当成"有规则"而不是"没有规则"，否则会把它装到所有平台上。
        let single_obj = serde_json::json!({"action": "allow", "os": {"name": "osx"}});
        assert!(!rules_allow(Some(&single_obj), feat));
        let single_win = serde_json::json!({"action": "allow", "os": {"name": "windows"}});
        assert!(rules_allow(Some(&single_win), feat));
    }

    #[test]
    fn args_from_string_honours_quotes_and_placeholders() {
        let p = ph();
        let toks = args_from_string(r#"--username ${auth_player_name} --gameDir "${game_directory}""#, &p)
            .unwrap();
        let texts: Vec<&str> = toks.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, vec!["--username", "CloudPlayer", "--gameDir", r"C:\g\saves\default"]);
    }

    #[test]
    fn args_from_array_skips_unmatched_rules() {
        let p = ph();
        let arr = serde_json::json!([
            "--width",
            {"rules": [{"action": "allow", "features": {"has_custom_resolution": true}}],
             "value": ["--width", "1920"]},
            {"rules": [{"action": "allow", "os": {"name": "osx"}}], "value": "--macOnly"}
        ]);
        let on = args_from_array(&arr, &p, Features { custom_resolution: true, ..Default::default() })
            .unwrap();
        let texts: Vec<&str> = on.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, vec!["--width", "--width", "1920"]);
        let off = args_from_array(&arr, &p, Features::default()).unwrap();
        let texts: Vec<&str> = off.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, vec!["--width"], "rules 不通过的元素整条不出现");
    }

    #[test]
    fn override_keeps_the_last_pair_only() {
        let toks = vec![
            ArgToken::json("--width"),
            ArgToken::json("1280"),
            ArgToken::json("--username"),
            ArgToken::json("A"),
            ArgToken::json("--width"),
            ArgToken::json("1920"),
            ArgToken::json("--demo"),
        ];
        let (out, over) = override_flags(toks);
        let texts: Vec<&str> = out.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, vec!["--username", "A", "--width", "1920", "--demo"]);
        assert_eq!(over, vec!["--width"]);
        assert!(duplicate_flags(&out).is_empty(), "去重之后不该再有重复");
    }

    #[test]
    fn override_leaves_the_no_value_flag_alone() {
        let toks = vec![
            ArgToken::json("--demo"),
            ArgToken::json("--width"),
            ArgToken::json("800"),
        ];
        let (out, over) = override_flags(toks);
        assert_eq!(out.len(), 3, "无值开关后面的键不能被当成它的值吞掉");
        assert!(over.is_empty());
    }

    #[test]
    fn empty_value_drops_the_key_too() {
        let toks = vec![
            ArgToken::json("--versionType"),
            ArgToken::json(""),
            ArgToken::json("--username"),
            ArgToken::json("A"),
        ];
        let texts: Vec<String> =
            drop_empty_values(toks).into_iter().map(|t| t.text).collect();
        assert_eq!(texts, vec!["--username", "A"]);
    }
}
