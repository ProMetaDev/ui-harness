//! 工作流：把多步流程写成文件，由内核按顺序（或并行）跑，而不是指望模型自己记住。
//!
//! 每个 Task 就是一次普通的 `run_turn`——预算、策略、日志、审批全都不用重写。
//! Group 里的 Task 并行执行：每个跑在**独立进程 + 独立日志**里（`delegate`），
//! 父日志只记「派了谁、结果如何」，不会出现两个进程抢同一个 journal。

use crate::json::{self, Json};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Default)]
pub struct Task {
    pub prompt: String,
    /// 这一步之前必须有人批准
    pub gate: bool,
    /// 这一步开始前自动预加载的 skill（正文进上下文并留痕）
    pub skills: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum Node {
    Task(Task),
    /// 并行子代理：互不阻塞，各有各的日志
    Group(Vec<Task>),
}

#[derive(Debug, Clone)]
pub struct Workflow {
    pub name: String,
    pub steps: Vec<Node>,
}

fn parse_task(raw: &Json, index: usize) -> Result<Task, String> {
    let prompt = raw
        .str_at("prompt")
        .ok_or_else(|| format!("第 {} 步缺少 prompt", index + 1))?
        .to_string();
    let skills = raw
        .arr_at("skills")
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Ok(Task {
        prompt,
        gate: raw.bool_at("gate").unwrap_or(false),
        skills,
    })
}

pub fn load(path: &Path) -> Result<Workflow, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("读工作流失败: {}", e))?;
    let value = json::parse(&text).map_err(|e| format!("工作流不是合法 JSON: {}", e))?;
    let name = value.str_at("name").unwrap_or("workflow").to_string();
    let arr = value
        .arr_at("steps")
        .ok_or_else(|| "工作流缺少 steps 数组".to_string())?;

    let mut steps = Vec::new();
    for (i, raw) in arr.iter().enumerate() {
        if let Some(group) = raw.arr_at("parallel") {
            if group.is_empty() {
                return Err(format!("第 {} 步的 parallel 是空的", i + 1));
            }
            let tasks = group
                .iter()
                .enumerate()
                .map(|(j, t)| parse_task(t, i * 100 + j))
                .collect::<Result<Vec<_>, _>>()?;
            steps.push(Node::Group(tasks));
        } else {
            steps.push(Node::Task(parse_task(raw, i)?));
        }
    }
    if steps.is_empty() {
        return Err("工作流没有步骤".to_string());
    }
    Ok(Workflow { name, steps })
}

impl Workflow {
    pub fn task_count(&self) -> usize {
        self.steps
            .iter()
            .map(|n| match n {
                Node::Task(_) => 1,
                Node::Group(t) => t.len(),
            })
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ui-wf-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn loads_tasks_gates_and_skills() {
        let d = tmp("load");
        let f = d.join("wf.json");
        fs::write(
            &f,
            r#"{"name":"review","steps":[
                {"prompt":"先读 diff","skills":["security"]},
                {"prompt":"现在你来评审","gate":true}
            ]}"#,
        )
        .unwrap();
        let wf = load(&f).unwrap();
        assert_eq!(wf.name, "review");
        assert_eq!(wf.steps.len(), 2);
        match &wf.steps[0] {
            Node::Task(t) => {
                assert_eq!(t.skills, vec!["security".to_string()]);
                assert!(!t.gate);
            }
            _ => panic!("应该是普通任务"),
        }
        match &wf.steps[1] {
            Node::Task(t) => assert!(t.gate),
            _ => panic!("应该是普通任务"),
        }
    }

    #[test]
    fn loads_parallel_group() {
        let d = tmp("parallel");
        let f = d.join("wf.json");
        fs::write(
            &f,
            r#"{"name":"p","steps":[
                {"parallel":[
                    {"prompt":"从安全角度评审","skills":["security"]},
                    {"prompt":"从性能角度评审"}
                ]}
            ]}"#,
        )
        .unwrap();
        let wf = load(&f).unwrap();
        assert_eq!(wf.steps.len(), 1);
        assert_eq!(wf.task_count(), 2);
        match &wf.steps[0] {
            Node::Group(t) => {
                assert_eq!(t.len(), 2);
                assert_eq!(t[0].skills, vec!["security".to_string()]);
            }
            _ => panic!("应该是并行组"),
        }
    }

    #[test]
    fn rejects_empty_or_broken_files() {
        let d = tmp("bad");
        let f = d.join("wf.json");
        fs::write(&f, r#"{"name":"x","steps":[]}"#).unwrap();
        assert!(load(&f).is_err());
        fs::write(&f, "not json").unwrap();
        assert!(load(&f).is_err());
        fs::write(&f, r#"{"name":"x","steps":[{"parallel":[]}]}"#).unwrap();
        assert!(load(&f).is_err());
    }
}
