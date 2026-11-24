//! 録画中だけ出力機器の消音を外して音量を 0 にする判断（Windows の `AppRule::system_audio`。D37・D38）。
//! OS 層が出力機器を読み書きし、ここは「いつ・何をするか」だけを決める。

/// 音量（0〜1）がこれ以下なら 0 とみなす。
pub const ZERO_EPS: f32 = 0.005;
/// 0 から 1 回でこれ以上に上がったら、会議アプリが上げたとみなす（Teams は 20%、音量キーは 2% ずつ。実機で確認）。
pub const JUMP: f32 = 0.1;

/// 音量が 0 から一気に上がったか（消音中は数えない）。
pub fn is_jump(prev: f32, now: f32, muted: bool) -> bool {
    !muted && prev <= ZERO_EPS && now >= JUMP
}

/// 利用者が自分で音量を決めたか（少しずつ動かした・0 以外から変えた）。
/// 一気に上がった分は、会議アプリが上げたものと区別できないので含めない。
pub fn is_user_set(prev: f32, now: f32, muted: bool) -> bool {
    !muted && now > ZERO_EPS && !is_jump(prev, now, muted)
}

/// 取り込みを終えるときに、出力機器をどう戻すか。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Restore {
    /// 消音を外していない。
    Untouched,
    /// 利用者が音量を上げていた（か、音量を読めない）ので、そのままにする。
    Keep,
    /// 消音にして、この音量に戻す。
    Remute(f32),
}

/// 1 つの出力機器の状態。
#[derive(Debug, Default)]
pub struct OutputUnmute {
    /// 録画が終わったら戻す音量。消音を外していなければ None。
    saved: Option<f32>,
    /// 次に音量が一気に上げられたら 0 に戻す。
    revert_next: bool,
    /// 会議に入ってしばらく経った（以後は戻さない）。
    settled: bool,
}

impl OutputUnmute {
    /// 消音を外して音量を 0 にした。`level` は外す直前の音量（0 でなければ、利用者が選び直した音量として覚え直す）。
    pub fn unmuted(&mut self, level: f32) {
        if level > ZERO_EPS || self.saved.is_none() {
            self.saved = Some(level);
        }
        self.revert_next = !self.settled;
    }

    /// 音量が一気に上げられた。0 に戻すなら true（消音を外すたびに 1 回だけ。`user_set`・`settle` のあとは戻さない）。
    pub fn jumped(&mut self) -> bool {
        std::mem::take(&mut self.revert_next)
    }

    /// 利用者が自分で音量を決めた。次に消音を外すまで、音量には触らない。
    pub fn user_set(&mut self) {
        self.revert_next = false;
    }

    /// 会議に入ってしばらく経った。以後は音量を戻さない（消音を外すのは続ける）。
    pub fn settle(&mut self) {
        self.settled = true;
        self.revert_next = false;
    }

    /// 取り込みを終える。`now` は今の音量（読めなければ NaN）。
    pub fn finish(&mut self, now: f32) -> Restore {
        self.revert_next = false;
        match self.saved.take() {
            None => Restore::Untouched,
            Some(level) if now <= ZERO_EPS => Restore::Remute(level),
            Some(_) => Restore::Keep,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jump_is_a_single_large_step_from_zero_while_unmuted() {
        assert!(is_jump(0.0, 0.2, false));
        assert!(is_jump(0.0, 0.35, false));
        // 音量キー（2% ずつ）や、0 以外からの変化は当たらない。
        assert!(!is_jump(0.0, 0.02, false));
        assert!(!is_jump(0.02, 0.04, false));
        assert!(!is_jump(0.3, 0.6, false));
        // 消音中は音が出ないので数えない（録画の終わりに元の音量へ戻すときなど）。
        assert!(!is_jump(0.0, 0.41, true));
        assert!(!is_jump(0.0, f32::NAN, false));
    }

    #[test]
    fn user_set_is_a_small_step_or_a_change_from_nonzero() {
        // 音量キー（2% ずつ）やスライダーのドラッグ、0 以外からの変化。
        assert!(is_user_set(0.0, 0.02, false));
        assert!(is_user_set(0.02, 0.04, false));
        assert!(is_user_set(0.3, 0.6, false));
        assert!(is_user_set(0.3, 0.25, false));
        // 一気に上がった分は、会議アプリが上げたものと区別できない。
        assert!(!is_user_set(0.0, 0.2, false));
        // MeetRec 自身の操作（音量 0 にする・消音中の変化）は当たらない。
        assert!(!is_user_set(0.41, 0.0, true));
        assert!(!is_user_set(0.2, 0.0, false));
        assert!(!is_user_set(0.0, 0.41, true));
        assert!(!is_user_set(0.0, f32::NAN, false));
    }

    #[test]
    fn user_set_before_joining_keeps_the_volume() {
        let mut u = OutputUnmute::default();
        u.unmuted(0.41);
        u.user_set();
        assert!(!u.jumped());
        // もう一度消音にされて外したら、数え直す（音を出したくない、に戻ったので）。
        u.unmuted(0.3);
        assert!(u.jumped());
        assert_eq!(u.finish(0.0), Restore::Remute(0.3));
    }

    #[test]
    fn nothing_is_reverted_after_the_meeting_has_settled() {
        let mut u = OutputUnmute::default();
        u.unmuted(0.41);
        u.settle();
        assert!(!u.jumped());
        // 録画中に消音にし直されて外しても、もう戻さない（消音を外すのは続ける）。
        u.unmuted(0.0);
        assert!(!u.jumped());
        assert_eq!(u.finish(0.0), Restore::Remute(0.41));
    }

    #[test]
    fn nothing_to_do_until_unmuted() {
        let mut u = OutputUnmute::default();
        assert!(!u.jumped());
        assert_eq!(u.finish(0.0), Restore::Untouched);
    }

    #[test]
    fn only_the_first_jump_after_unmuting_is_reverted() {
        let mut u = OutputUnmute::default();
        u.unmuted(0.41);
        assert!(u.jumped());
        assert!(!u.jumped());
        assert!(!u.jumped());
        // 利用者が消音にし直し、もう一度外したら、数え直す。
        u.unmuted(0.0);
        assert!(u.jumped());
        assert!(!u.jumped());
    }

    #[test]
    fn finish_restores_the_first_level_only_if_volume_is_still_zero() {
        let mut u = OutputUnmute::default();
        u.unmuted(0.41);
        // 消音にし直されて 2 回目に外したとき（直前の音量は 0）も、最初の音量を覚えている。
        u.unmuted(0.0);
        assert_eq!(u.finish(0.0), Restore::Remute(0.41));
        // 戻したあとは何も残らない（出力機器を開き直したときに二重に戻さない）。
        assert_eq!(u.finish(0.0), Restore::Untouched);
        assert!(!u.jumped());

        // 利用者が自分で音量を上げていたら、戻さない。今の音量を読めないときも同じ。
        let mut u = OutputUnmute::default();
        u.unmuted(0.41);
        assert_eq!(u.finish(0.2), Restore::Keep);
        assert_eq!(u.finish(0.0), Restore::Untouched);
        u.unmuted(0.41);
        assert_eq!(u.finish(f32::NAN), Restore::Keep);
    }
}
