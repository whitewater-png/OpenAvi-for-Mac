//! Undo / Redo 用の履歴。編集の種類を問わず「スナップショットの差分」で記録する方式。
//!
//! UI の各所が `scene` を直接書き換えるため、個々の操作を Command にせず、毎フレーム「前回確定した状態」と
//! 見比べる。ドラッグ中などは `stable == false` で確定を保留し、ボタンを離した時点で 1 回だけ記録する
//! （= 1 回のドラッグ操作が 1 回の Undo になる）。

/// 履歴の上限（古いものから捨てる）。
const LIMIT: usize = 200;

pub struct History<T> {
    undo: Vec<T>,
    redo: Vec<T>,
    /// 直近に確定した状態
    committed: T,
}

impl<T: Clone + PartialEq> History<T> {
    pub fn new(initial: &T) -> Self {
        Self { undo: Vec::new(), redo: Vec::new(), committed: initial.clone() }
    }

    /// ファイルを開いた直後など、履歴を捨てて `initial` を起点にし直す。
    pub fn reset(&mut self, initial: &T) {
        *self = Self::new(initial);
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// 毎フレーム呼ぶ。`stable` が真（操作の途中でない）で状態が変わっていれば履歴に積む。積んだら `true`。
    pub fn observe(&mut self, current: &T, stable: bool) -> bool {
        if !stable || *current == self.committed {
            return false;
        }
        self.undo.push(std::mem::replace(&mut self.committed, current.clone()));
        if self.undo.len() > LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
        true
    }

    /// 1 つ前の状態へ戻す。操作の途中の変更があれば、それも 1 回分として扱う。
    pub fn undo(&mut self, current: &mut T) -> bool {
        self.observe(current, true);
        let Some(prev) = self.undo.pop() else { return false };
        self.redo.push(std::mem::replace(&mut self.committed, prev.clone()));
        *current = prev;
        true
    }

    pub fn redo(&mut self, current: &mut T) -> bool {
        self.observe(current, true);
        let Some(next) = self.redo.pop() else { return false };
        self.undo.push(std::mem::replace(&mut self.committed, next.clone()));
        *current = next;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_only_stable_changes_and_undoes_in_order() {
        let mut state = 0;
        let mut h = History::new(&state);
        assert!(!h.observe(&state, true), "変化なしは記録しない");

        state = 1;
        assert!(!h.observe(&state, false), "操作中は保留");
        state = 2; // ドラッグ中に値が動き続けても
        assert!(!h.observe(&state, false));
        assert!(h.observe(&state, true), "離したら 1 回だけ記録");
        state = 3;
        assert!(h.observe(&state, true));

        assert!(h.undo(&mut state));
        assert_eq!(state, 2);
        assert!(h.undo(&mut state));
        assert_eq!(state, 0, "ドラッグ全体が 1 回の Undo（1,2 は途中経過）");
        assert!(!h.undo(&mut state), "これ以上戻れない");

        assert!(h.redo(&mut state));
        assert_eq!(state, 2);
        assert!(h.redo(&mut state));
        assert_eq!(state, 3);
        assert!(!h.redo(&mut state));
    }

    #[test]
    fn new_edit_clears_redo_and_pending_change_is_kept_on_undo() {
        let mut state = 0;
        let mut h = History::new(&state);
        state = 1;
        h.observe(&state, true);
        h.undo(&mut state);
        assert!(h.can_redo());
        state = 5; // Undo 後に別の編集
        h.observe(&state, true);
        assert!(!h.can_redo(), "新しい編集で Redo は消える");

        // 未確定の変更（操作の途中）があるまま Undo → その変更も 1 回分として戻せる
        state = 9;
        h.undo(&mut state);
        assert_eq!(state, 5);
        h.redo(&mut state);
        assert_eq!(state, 9);
    }

    #[test]
    fn reset_and_limit() {
        let mut state = 0usize;
        let mut h = History::new(&state);
        for i in 1..=LIMIT + 50 {
            state = i;
            h.observe(&state, true);
        }
        let mut n = 0;
        while h.undo(&mut state) {
            n += 1;
        }
        assert_eq!(n, LIMIT, "上限を超えた分は捨てる");
        h.reset(&state);
        assert!(!h.can_undo() && !h.can_redo());
    }
}
