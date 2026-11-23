//! 「PC から出る音すべて」の録画（Windows。`AppRule::system_audio`）で、出力機器の消音を外して音量を 0 にする判断（D37）。
//!
//! 出力機器が消音だと音を出すのをやめる会議アプリがある（Teams で実機確認）。音量 0（消音ではない）なら出し続けるので、
//! 録っているあいだだけ切り替え、終わったら元に戻す。OS 層が出力機器を読み書きし、ここは「いつ・何をするか」だけを決める。

/// 出力機器の音量（0〜1）がこれ以下なら「0 のまま」とみなす。
pub const ZERO_EPS: f32 = 0.005;
/// 音量が 0 から 1 回の変化でこれ以上に上がったら、アプリが上げたものとみなす
/// （実機で確認：Teams は、音量 0 のまま会議に入ると 20% に上げる。音量キーは 2% ずつ上がる）。
pub const JUMP: f32 = 0.1;

/// 音量の 1 回の変化（`prev` → `now`）が「0 から一気に上がった」に当たるか。消音中の変化は音が出ないので数えない。
pub fn is_jump(prev: f32, now: f32, muted: bool) -> bool {
    !muted && prev <= ZERO_EPS && now >= JUMP
}

/// 取り込みを終えるときに、出力機器をどう戻すか。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Restore {
    /// 消音を外していないので、何もしない。
    Untouched,
    /// 利用者が自分で音量を上げていた（か、今の音量を読めない）ので、そのままにする。聞きたくて上げた音を消さない。
    Keep,
    /// 消音にして、この音量（消音を外す前の値）に戻す。
    Remute(f32),
}

/// 1 つの出力機器について、消音を外したかどうかと、元に戻すための値を覚える。
#[derive(Debug, Default)]
pub struct OutputUnmute {
    /// 消音を外す前の音量（0〜1）。まだ外していなければ None。
    saved: Option<f32>,
    /// 次に音量が一気に上げられたら 0 に戻す（消音を外すたびに 1 回だけ）。
    revert_next: bool,
}

impl OutputUnmute {
    /// 消音を外して音量を 0 にした。`level` は外す直前の音量。
    /// 利用者が消音にし直したあとの 2 回目以降は、最初に覚えた音量を残す（2 回目の直前の音量は 0 なので）。
    pub fn unmuted(&mut self, level: f32) {
        self.saved.get_or_insert(level);
        self.revert_next = true;
    }

    /// 音量が 0 から一気に上げられた（`is_jump`）。0 に戻すなら true。
    /// 消音を外したあとの最初の 1 回だけ戻す（会議に入るときに会議アプリが上げる分）。2 回目からは、
    /// 利用者がスライダーで上げたものとして、そのままにする。
    pub fn jumped(&mut self) -> bool {
        std::mem::take(&mut self.revert_next)
    }

    /// 取り込みを終える。`now` は今の音量（読めなければ NaN）。出力機器をどう戻すかを返す。
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
        // 実機で確認：Teams は、音量 0 のまま会議に入ると 20% に上げる。
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
    fn nothing_to_do_until_unmuted() {
        let mut u = OutputUnmute::default();
        // 消音を外していないあいだは、利用者の音量を触らない。
        assert!(!u.jumped());
        assert_eq!(u.finish(0.0), Restore::Untouched);
    }

    #[test]
    fn only_the_first_jump_after_unmuting_is_reverted() {
        let mut u = OutputUnmute::default();
        u.unmuted(0.41);
        assert!(u.jumped());
        // 2 回目からは利用者の操作として通す。
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
