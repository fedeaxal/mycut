//! Aho-Corasick 违禁词自动机（Layer1）。
//! 与渲染端旧 scanner.ts 的 JS 实现 1:1 同构：字符粒度（char）、构建期沿
//! fail 链合并 outputs（匹配时零回溯）、root 的 fail 恒为自身（下标 0）。

use std::collections::{HashMap, VecDeque};

struct AcNode {
    children: HashMap<char, usize>,
    /// 失配回退节点；root 的 fail 恒为 0（自身）。
    fail: usize,
    /// 该节点收尾的词（构建时已沿 fail 链合并）与词的字符长度。
    outputs: Vec<(String, usize)>,
}

pub struct Automaton {
    nodes: Vec<AcNode>,
}

impl Automaton {
    pub fn build(words: &[String]) -> Automaton {
        let mut nodes = vec![AcNode {
            children: HashMap::new(),
            fail: 0,
            outputs: Vec::new(),
        }];
        for word in words {
            if word.is_empty() {
                continue;
            }
            let mut node = 0usize;
            for ch in word.chars() {
                let next = nodes[node].children.get(&ch).copied();
                node = match next {
                    Some(n) => n,
                    None => {
                        nodes.push(AcNode {
                            children: HashMap::new(),
                            fail: 0,
                            outputs: Vec::new(),
                        });
                        let fresh = nodes.len() - 1;
                        nodes[node].children.insert(ch, fresh);
                        fresh
                    }
                };
            }
            nodes[node].outputs.push((word.clone(), word.chars().count()));
        }
        // BFS 建 fail 指针并合并输出（子 fail = 父 fail 的同名子节点）
        let mut queue: VecDeque<usize> = VecDeque::new();
        for child in nodes[0].children.values().copied().collect::<Vec<_>>() {
            nodes[child].fail = 0;
            queue.push_back(child);
        }
        while let Some(node) = queue.pop_front() {
            for (ch, child) in nodes[node].children.clone() {
                let mut fail = nodes[node].fail;
                while fail != 0 && !nodes[fail].children.contains_key(&ch) {
                    fail = nodes[fail].fail;
                }
                let fallback = nodes[fail].children.get(&ch).copied();
                let merged = match fallback {
                    // 与 JS 一致：fallback 是自己（root 的直接子节点）时归 root
                    Some(f) if f != child => f,
                    _ => 0,
                };
                nodes[child].fail = merged;
                if !nodes[merged].outputs.is_empty() {
                    let merged_outputs = nodes[merged].outputs.clone();
                    nodes[child].outputs.extend(merged_outputs);
                }
                queue.push_back(child);
            }
        }
        Automaton { nodes }
    }

    /// 返回 (命中词, 词首字符下标)；遍历顺序与 JS 逐字符扫描一致。
    pub fn find<'a>(&'a self, text: &'a str) -> Vec<(&'a str, usize)> {
        let mut hits = Vec::new();
        let mut node = 0usize;
        for (index, ch) in text.chars().enumerate() {
            while node != 0 && !self.nodes[node].children.contains_key(&ch) {
                node = self.nodes[node].fail;
            }
            node = self.nodes[node].children.get(&ch).copied().unwrap_or(0);
            for (word, len) in &self.nodes[node].outputs {
                hits.push((word.as_str(), index + 1 - len));
            }
        }
        hits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn finds_overlapping_words() {
        let auto = Automaton::build(&words(&["赌博", "博彩", "线上赌博"]));
        let mut hits = auto.find("线上赌博与博彩");
        hits.sort();
        let pairs: Vec<(&str, usize)> = hits.iter().map(|(w, i)| (*w, *i)).collect();
        assert_eq!(pairs, vec![("博彩", 5), ("线上赌博", 0), ("赌博", 2)]);
    }

    #[test]
    fn shares_suffix_fail_links() {
        // 「诈骗」命中依赖 fail 链合并输出（在「网络诈骗」词尾一并吐出）
        let auto = Automaton::build(&words(&["网络诈骗", "诈骗"]));
        let pairs: Vec<(&str, usize)> =
            auto.find("网络诈骗").iter().map(|(w, i)| (*w, *i)).collect();
        assert_eq!(pairs, vec![("网络诈骗", 0), ("诈骗", 2)]);
    }

    #[test]
    fn empty_input_no_hits() {
        let auto = Automaton::build(&words(&["赌博"]));
        assert!(auto.find("").is_empty());
    }
}
