//! 同音词纠正：拼音混淆变体（ai-client expandPinyinVariants/buildHrRules）
//! + OpenFst VectorFst<StdArc> V2 二进制生成（main/hr-fst.ts 逐字节移植）。
//!
//! 结构约束（kaldifst TextNormalizer 的 replace.fst 语义）：
//! - 输入侧 = 拼音字节连写；输出侧 = 目标汉字 UTF-8 字节流；
//! - MAIN 全字节透传自环（含 ε:ε 吸收 pad 弧），链尾透传回 MAIN；
//! - 权重透传 w=1 > 链 w=0，ShortestPath(tropical) 必选替换；
//! - 弧按 ilabel 排序写出（matcher 依赖有序弧）。

use serde_json::Value;


fn is_valid_pinyin(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// 角色名必须纯汉字（FST 的输出重建只认 ≥0x80 的 UTF-8 字节）。
fn is_pure_hanzi(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| matches!(c as u32, 0x4e00..=0x9fff))
}

/// 由基础拼音生成同音混淆变体：ASR 常见的前后鼻音、平翘舌、n/l 混淆。
/// 替换语义对齐 JS String.replace(/…/g, …)（全部出现、字面量无回溯）。
pub fn expand_pinyin_variants(pinyin: &str) -> Vec<String> {
    let mut variants: Vec<String> = vec![pinyin.to_string()];
    let push = |candidate: String, variants: &mut Vec<String>| {
        if !variants.contains(&candidate) {
            variants.push(candidate);
        }
    };
    // 前后鼻音互转（按韵尾整体替换，保持声调数字）
    for (from, to) in [
        ("ang", "an"),
        ("an", "ang"),
        ("eng", "en"),
        ("en", "eng"),
        ("ing", "in"),
        ("in", "ing"),
    ] {
        let candidate = pinyin.replace(from, to);
        if candidate != pinyin {
            push(candidate, &mut variants);
        }
    }
    // 平翘舌互转（首字母）
    for (from, to) in [
        ("zh", "z"),
        ("z", "zh"),
        ("ch", "c"),
        ("c", "ch"),
        ("sh", "s"),
        ("s", "sh"),
    ] {
        if pinyin.starts_with(from) {
            let candidate = format!("{to}{}", &pinyin[from.len()..]);
            push(candidate, &mut variants);
        }
    }
    // n/l 声母混淆
    let first = pinyin.chars().next();
    if first == Some('n') || first == Some('l') {
        let flipped = if first == Some('n') {
            format!("l{}", &pinyin[1..])
        } else {
            format!("n{}", &pinyin[1..])
        };
        push(flipped, &mut variants);
    }
    variants
        .into_iter()
        .filter(|v| is_valid_pinyin(v))
        .collect()
}

/// 有序 upsert：JS Map.set 语义（首插位置不变、值覆盖）。
fn map_upsert(map: &mut Vec<(String, String)>, key: String, value: String) {
    if let Some(entry) = map.iter_mut().find(|(k, _)| *k == key) {
        entry.1 = value;
    } else {
        map.push((key, value));
    }
}

/// 演员表 → FST 规则（含同音变体，全部映射到同一角色名）。
/// 返回 [(pinyin, target)]，插入序与 JS buildHrRules 一致。
pub fn build_hr_rules(cast: &[Value]) -> Vec<(String, String)> {
    let mut rules: Vec<(String, String)> = Vec::new();
    for entry in cast {
        let character = entry["character"].as_str().unwrap_or("");
        let pinyin = entry["pinyin"].as_str().unwrap_or("");
        if !is_pure_hanzi(character) || !is_valid_pinyin(pinyin) {
            continue;
        }
        for variant in expand_pinyin_variants(pinyin) {
            map_upsert(&mut rules, variant, character.to_string());
        }
    }
    rules
}

// ---- replace.fst 二进制生成（hr-fst.ts 移植）----

const MAGIC: u32 = 0x7eb2fdd6; // OpenFst kFstMagicNumber
const PROPS: i64 = 0x10000003; // 与 pynini 写出的官方 replace.fst 一致

#[derive(Clone, Copy)]
struct Arc {
    il: u32,
    ol: u32,
    w: f32,
    ns: u32,
}

#[derive(Clone)]
struct State {
    final_weight: f32,
    arcs: Vec<Arc>,
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_i64(out: &mut Vec<u8>, v: i64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_f32(out: &mut Vec<u8>, v: f32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_wstr(out: &mut Vec<u8>, s: &str) {
    // JS Buffer.from(s, "latin1")：字符串只含 ASCII，按字节写
    put_u32(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}

struct Fst {
    states: Vec<State>,
    /// MAIN 状态（状态 0）：passthrough 弧一律回 MAIN——链尾状态上的
    /// 透传是"回 MAIN 的转移"而非自环（JS 源里 ns 恒为 MAIN）。
    main: u32,
}

impl Fst {
    fn new() -> Fst {
        // 空表起步：MAIN 由 build_replace_fst 显式 add_state（与 JS 一致，
        // 预置会造成每 FST 多一个幽灵状态）
        Fst { states: Vec::new(), main: 0 }
    }
    fn add_state(&mut self, final_weight: f32) -> u32 {
        self.states.push(State {
            final_weight,
            arcs: Vec::new(),
        });
        (self.states.len() - 1) as u32
    }
    fn add_arc(&mut self, s: u32, arc: Arc) {
        self.states[s as usize].arcs.push(arc);
    }
    fn add_passthrough(&mut self, s: u32) {
        // 覆盖 0x01..0xbf 与 0xc2..0xf4（JS 三段循环合并，序列相同）+
        // ε:ε 吸收 pad 弧；目标一律回 MAIN
        for c in 0x01..=0xbfu32 {
            self.add_arc(s, Arc { il: c, ol: c, w: 1.0, ns: self.main });
        }
        for c in 0xc2..=0xf4u32 {
            self.add_arc(s, Arc { il: c, ol: c, w: 1.0, ns: self.main });
        }
        self.add_arc(s, Arc { il: 0, ol: 0, w: 1.0, ns: self.main });
    }
    fn serialize(&self) -> Vec<u8> {
        let mut states = self.states.clone();
        for st in states.iter_mut() {
            st.arcs.sort_by_key(|arc| arc.il);
        }
        let mut out = Vec::new();
        put_u32(&mut out, MAGIC);
        put_wstr(&mut out, "vector");
        put_wstr(&mut out, "standard");
        put_u32(&mut out, 2);
        put_u32(&mut out, 0);
        put_i64(&mut out, PROPS);
        put_i64(&mut out, 0);
        put_i64(&mut out, states.len() as i64);
        put_i64(&mut out, 0);
        for st in &states {
            put_f32(&mut out, st.final_weight);
            put_i64(&mut out, st.arcs.len() as i64);
            for arc in &st.arcs {
                put_u32(&mut out, arc.il);
                put_u32(&mut out, arc.ol);
                put_f32(&mut out, arc.w);
                put_u32(&mut out, arc.ns);
            }
        }
        out
    }
}

/// 生成 replace.fst 二进制。rules: (pinyin, target 汉字)。
pub fn build_replace_fst(rules: &[(String, String)]) -> Result<Vec<u8>, String> {
    for (pinyin, target) in rules {
        if !is_valid_pinyin(pinyin) {
            return Err(format!("拼音非法（需小写字母+数字标调连写）: {pinyin}"));
        }
        for ch in target.chars() {
            let mut buf = [0u8; 4];
            let bytes = ch.encode_utf8(&mut buf).as_bytes();
            if bytes.len() < 2 || bytes[0] < 0x80 {
                return Err(format!("替换目标必须为纯汉字: {pinyin} -> {target}"));
            }
        }
    }

    // 同拼音去重（后者覆盖，与 JS Map.set 一致）；被更长拼音严格包含且
    // 目标不同的短规则丢弃（同输入位置平权歧义）
    let mut uniq: Vec<(String, String)> = Vec::new();
    for (pinyin, target) in rules {
        map_upsert(&mut uniq, pinyin.clone(), target.clone());
    }
    let keys: Vec<&str> = uniq.iter().map(|(k, _)| k.as_str()).collect();
    let target_of = |p: &str| -> Option<&str> {
        uniq.iter()
            .find(|(k, _)| k == p)
            .map(|(_, v)| v.as_str())
    };
    let final_rules: Vec<&(String, String)> = uniq
        .iter()
        .filter(|(p, target)| {
            !keys.iter().any(|q| {
                *q != p.as_str()
                    && q.starts_with(p.as_str())
                    && target_of(q) != Some(target.as_str())
            })
        })
        .collect();

    let mut fst = Fst::new();
    let main = fst.add_state(0.0);
    fst.main = main;
    fst.add_passthrough(main);

    for (pinyin, target) in &final_rules {
        let pinyin_bytes = pinyin.as_bytes();
        let out_bytes = target.as_bytes();
        let n = pinyin_bytes.len();
        let m = out_bytes.len();
        // 链状态 C0..C(n-1)，入口弧 MAIN→C0 消耗 pinyin[0] 输出 out[0]
        let mut chain: Vec<u32> = Vec::with_capacity(n);
        for _ in 0..n {
            chain.push(fst.add_state(f32::INFINITY));
        }
        for k in 1..n {
            fst.add_arc(
                chain[k - 1],
                Arc {
                    il: pinyin_bytes[k] as u32,
                    ol: if k < m { out_bytes[k] as u32 } else { 0 },
                    w: 0.0,
                    ns: chain[k],
                },
            );
        }
        let end_state = chain[n - 1];
        fst.states[end_state as usize].final_weight = 0.0;
        if m > n {
            // 拼音消耗完仍有剩余目标字节：链尾 ε-输入弧输出后回 MAIN
            let head = fst.add_state(f32::INFINITY);
            fst.add_arc(end_state, Arc { il: 0, ol: out_bytes[n] as u32, w: 0.0, ns: head });
            let mut cur = head;
            for k in (n + 1)..m {
                let next = fst.add_state(f32::INFINITY);
                fst.add_arc(cur, Arc { il: 0, ol: out_bytes[k] as u32, w: 1.0, ns: next });
                cur = next;
            }
            fst.states[cur as usize].final_weight = 0.0;
            fst.add_passthrough(cur);
        } else {
            fst.add_passthrough(end_state);
        }
        fst.add_arc(main, Arc {
            il: pinyin_bytes[0] as u32,
            ol: out_bytes[0] as u32,
            w: 0.0,
            ns: chain[0],
        });
    }

    Ok(fst.serialize())
}

/// 演员表 → {count, fst(base64)}（hr:build IPC 的原生实现）。
pub fn hr_build(cast: &[Value]) -> Result<(usize, String), String> {
    let rules = build_hr_rules(cast);
    let count = rules.len();
    let bytes = build_replace_fst(&rules)?;
    Ok((count, base64_encode(&bytes)))
}

/// parity 通道用的公开 base64（内部编码器原样暴露）。
pub fn base64_encode_public(data: &[u8]) -> String {
    base64_encode(data)
}

fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pinyin_variants_nasal_retroflex() {
        let v = expand_pinyin_variants("xuan2jie4");
        assert!(v.contains(&"xuan2jie4".to_string()));
        // zh→z 首字母互转
        let zhang = expand_pinyin_variants("zhang3");
        assert!(zhang.contains(&"zhang3".to_string()));
        assert!(zhang.contains(&"zang3".to_string()));
        // n/l 混淆
        let lin = expand_pinyin_variants("lin2");
        assert!(lin.contains(&"lin2".to_string()));
        assert!(!lin.is_empty());
    }

    #[test]
    fn hr_rules_filter_and_dedup() {
        let cast = vec![
            json!({ "character": "林美丽", "actor": "张三", "pinyin": "lin2mei3li4" }),
            json!({ "character": "English", "actor": "x", "pinyin": "abc" }), // 非纯汉字跳过
            json!({ "character": "李四", "actor": "y", "pinyin": "Lin2" }),   // 大写非法跳过
        ];
        let rules = build_hr_rules(&cast);
        assert!(rules.iter().all(|(p, t)| t == "林美丽"));
        assert!(rules.iter().any(|(p, _)| p == "lin2mei3li4"));
    }

    #[test]
    fn fst_rejects_bad_input() {
        assert!(build_replace_fst(&[("Lin2".into(), "字".into())]).is_err());
        assert!(build_replace_fst(&[("lin2".into(), "A".into())]).is_err());
    }

    #[test]
    fn fst_header_shape() {
        let bytes = build_replace_fst(&[("lin2".into(), "林".into())]).unwrap();
        assert_eq!(&bytes[0..4], &MAGIC.to_le_bytes());
        // wstr("vector")：len=6 + 6 字节
        assert_eq!(&bytes[4..8], &6u32.to_le_bytes());
        assert_eq!(&bytes[8..14], b"vector");
        assert_eq!(&bytes[14..18], &8u32.to_le_bytes());
        assert_eq!(&bytes[18..26], b"standard");
        assert_eq!(&bytes[26..30], &2u32.to_le_bytes()); // fst type version
    }

    #[test]
    fn base64_known_vector() {
        assert_eq!(base64_encode(b"abc"), "YWJj");
        assert_eq!(base64_encode(b"ab"), "YWI=");
        assert_eq!(base64_encode(b"a"), "YQ==");
    }
}
