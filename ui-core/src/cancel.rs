//! 取消令牌：一个跨线程的布尔，够用就行。
//!
//! 为什么需要它：模型调用可能持续几十秒，这期间 UI 线程是阻塞的。
//! 没有令牌，用户只能等；有了它，后台读键线程置位，模型适配器轮询到就杀掉子进程。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone, Default)]
pub struct Cancel {
    flag: Arc<AtomicBool>,
    /// 「硬取消」：外部原因（父进程死亡）设的，reset 清不掉。
    /// 否则 turn 开头的 reset 会把父进程死亡这件事抹掉，子进程继续跑。
    hard: Arc<AtomicBool>,
}

impl Cancel {
    pub fn new() -> Cancel {
        Cancel::default()
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    pub fn cancel_hard(&self) {
        self.hard.store(true, Ordering::SeqCst);
    }

    pub fn reset(&self) {
        self.flag.store(false, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst) || self.hard.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_share_one_flag() {
        let a = Cancel::new();
        let b = a.clone();
        assert!(!b.is_cancelled());
        a.cancel();
        assert!(b.is_cancelled());
        b.reset();
        assert!(!a.is_cancelled());
    }
}
