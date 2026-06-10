//! 持有人快照的保存与对比：diff 出谁在加仓、谁在出货、谁进谁出。

use crate::types::{Holder, SnapshotDiff};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct Snapshot {
    pub ts: i64,
    pub mint: String,
    /// (owner, balance)
    pub holders: Vec<(String, f64)>,
}

fn dir_for(mint: &str) -> String {
    format!("snapshots/{mint}")
}

pub fn save(mint: &str, holders: &[Holder], ts: i64) -> Result<String> {
    let dir = dir_for(mint);
    std::fs::create_dir_all(&dir)?;
    let snap = Snapshot {
        ts,
        mint: mint.to_string(),
        holders: holders
            .iter()
            .map(|h| (h.owner.clone(), h.balance))
            .collect(),
    };
    let path = format!("{dir}/{ts}.json");
    std::fs::write(&path, serde_json::to_string(&snap)?)?;
    Ok(path)
}

/// path = "latest" 时取该 mint 目录下最新的快照文件。
pub fn load(mint: &str, path: &str) -> Result<Snapshot> {
    let real_path = if path == "latest" {
        let dir = dir_for(mint);
        std::fs::read_dir(&dir)
            .with_context(|| format!("没有历史快照目录 {dir}，先用 --snapshot 保存一次"))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .max()
            .context("快照目录为空")?
            .to_string_lossy()
            .into_owned()
    } else {
        path.to_string()
    };
    let s = std::fs::read_to_string(&real_path)?;
    Ok(serde_json::from_str(&s)?)
}

pub fn diff(old: &Snapshot, holders: &[Holder]) -> SnapshotDiff {
    use std::collections::HashMap;
    let old_map: HashMap<&str, f64> = old
        .holders
        .iter()
        .map(|(o, b)| (o.as_str(), *b))
        .collect();
    let new_map: HashMap<&str, f64> =
        holders.iter().map(|h| (h.owner.as_str(), h.balance)).collect();

    let mut changes: Vec<(String, f64, f64)> = Vec::new();
    let mut new_holders = 0usize;
    let mut exited = 0usize;
    for (owner, &new_bal) in &new_map {
        let old_bal = old_map.get(owner).copied().unwrap_or(0.0);
        if old_bal == 0.0 && new_bal > 0.0 {
            new_holders += 1;
        }
        if (new_bal - old_bal).abs() > f64::EPSILON {
            changes.push((owner.to_string(), old_bal, new_bal));
        }
    }
    for (owner, &old_bal) in &old_map {
        if !new_map.contains_key(owner) && old_bal > 0.0 {
            exited += 1;
            changes.push((owner.to_string(), old_bal, 0.0));
        }
    }
    changes.sort_by(|a, b| {
        let da = (a.2 - a.1).abs();
        let db = (b.2 - b.1).abs();
        db.total_cmp(&da)
    });
    SnapshotDiff {
        base_time: old.ts,
        changes,
        new_holders,
        exited_holders: exited,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn holder(owner: &str, bal: f64) -> Holder {
        Holder {
            owner: owner.into(),
            token_accounts: vec![],
            balance: bal,
            pct: 0.0,
            label: None,
        }
    }

    #[test]
    fn diff_detects_changes() {
        let old = Snapshot {
            ts: 100,
            mint: "M".into(),
            holders: vec![("A".into(), 100.0), ("B".into(), 50.0), ("C".into(), 10.0)],
        };
        // A 加仓到 300, B 清仓退出, C 不变, D 新进 40
        let new = vec![holder("A", 300.0), holder("C", 10.0), holder("D", 40.0)];
        let d = diff(&old, &new);
        assert_eq!(d.new_holders, 1);
        assert_eq!(d.exited_holders, 1);
        // 变化量降序: A(+200), B(-50), D(+40)
        assert_eq!(d.changes[0].0, "A");
        assert_eq!(d.changes[1].0, "B");
        assert_eq!(d.changes[2].0, "D");
        assert_eq!(d.changes.len(), 3);
    }
}
