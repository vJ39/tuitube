//! 再生速度の値オブジェクト。mpv へ送る値と表示文字列はここだけで作る。

use crate::mpv::{self, MpvCommand};
use serde_json::json;

/// 0.1 倍刻みの整数 (十分の一単位) で持つ。浮動小数の累積誤差と範囲外を型で防ぐ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Speed(u8);

impl Speed {
    pub const MIN: Speed = Speed(1);
    pub const MAX: Speed = Speed(40);
    pub const NORMAL: Speed = Speed(10);

    #[allow(
        dead_code,
        reason = "値オブジェクトの基本 API。現状 production からは stepped 経由でしか作らない"
    )]
    pub fn from_tenths(tenths: u8) -> Option<Self> {
        (Self::MIN.0..=Self::MAX.0)
            .contains(&tenths)
            .then_some(Self(tenths))
    }

    #[allow(
        dead_code,
        reason = "範囲外を拒む方の変換。取り込みは Polled::from_f64 を使う"
    )]
    pub fn from_f64(value: f64) -> Option<Self> {
        let tenths = Self::to_tenths(value)?;
        u8::try_from(tenths).ok().and_then(Self::from_tenths)
    }

    pub fn stepped(self, steps: i8) -> Self {
        let next = i16::from(self.0) + i16::from(steps);
        Self(next.clamp(i16::from(Self::MIN.0), i16::from(Self::MAX.0)) as u8)
    }

    #[allow(dead_code, reason = "内部表現の取り出し口")]
    pub fn tenths(self) -> u8 {
        self.0
    }

    pub fn value(self) -> f64 {
        f64::from(self.0) / 10.0
    }

    pub fn decimal(self) -> String {
        format!("{}.{}", self.0 / 10, self.0 % 10)
    }

    pub fn label(self) -> String {
        format!("{}x", self.decimal())
    }

    pub fn command(self) -> MpvCommand {
        mpv::set_property("speed", json!(self.value()))
    }

    /// 等速は mpv の既定なので渡さない。
    pub fn launch_arg(self) -> Option<String> {
        (self != Self::NORMAL).then(|| format!("--speed={}", self.decimal()))
    }

    /// 0.1 刻みへ四捨五入する。有限でない値は None。
    fn to_tenths(value: f64) -> Option<i64> {
        value.is_finite().then(|| (value * 10.0).round() as i64)
    }
}

impl Default for Speed {
    fn default() -> Self {
        Self::NORMAL
    }
}

/// ポーリングで届いた速度。mpv ウィンドウ側で範囲外にされていたときは丸めた事実も持つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Polled {
    pub speed: Speed,
    /// 丸めたので、mpv へ送り返さないと表示と実際の再生が食い違う。
    pub clamped: bool,
}

impl Polled {
    pub fn from_f64(value: f64) -> Self {
        let Some(tenths) = Speed::to_tenths(value) else {
            return Self {
                speed: Speed::NORMAL,
                clamped: false,
            };
        };
        let bounded = tenths.clamp(i64::from(Speed::MIN.0), i64::from(Speed::MAX.0));
        Self {
            speed: Speed(bounded as u8),
            clamped: bounded != tenths,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn speed(tenths: u8) -> Speed {
        Speed::from_tenths(tenths).expect("範囲内")
    }

    #[test]
    fn speed_rejects_tenths_outside_one_to_forty() {
        assert_eq!(Speed::from_tenths(0), None);
        assert_eq!(Speed::from_tenths(41), None);
        assert_eq!(Speed::from_tenths(1), Some(Speed::MIN));
        assert_eq!(Speed::from_tenths(40), Some(Speed::MAX));
        assert_eq!(Speed::MIN.tenths(), 1);
        assert_eq!(Speed::MAX.tenths(), 40);
        assert_eq!(Speed::NORMAL.value(), 1.0);
        assert_eq!(Speed::default(), Speed::NORMAL);
    }

    #[test]
    fn speed_from_f64_rounds_to_a_tenth_and_rejects_out_of_range() {
        assert_eq!(Speed::from_f64(1.25), Some(speed(13)));
        assert_eq!(Speed::from_f64(0.04), None);
        assert_eq!(Speed::from_f64(0.05), Some(Speed::MIN));
        assert_eq!(Speed::from_f64(4.04), Some(Speed::MAX));
        assert_eq!(Speed::from_f64(4.05), None);
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
            assert_eq!(Speed::from_f64(value), None, "{value}");
        }
    }

    fn polled(speed: Speed, clamped: bool) -> Polled {
        Polled { speed, clamped }
    }

    #[test]
    fn a_polled_value_is_rounded_and_reports_whether_it_was_clamped() {
        // mpv ウィンドウ側の × 1.1 は 0.1 刻みに乗らない値になる。丸めるだけで範囲内。
        assert_eq!(Polled::from_f64(2.75), polled(speed(28), false));
        assert_eq!(Polled::from_f64(4.0), polled(Speed::MAX, false));
        // 範囲外は丸めたことを伝える。呼び出し側が mpv へ送り返す。
        assert_eq!(Polled::from_f64(8.0), polled(Speed::MAX, true));
        assert_eq!(Polled::from_f64(0.01), polled(Speed::MIN, true));
        assert_eq!(Polled::from_f64(f64::NAN), polled(Speed::NORMAL, false));
    }

    #[test]
    fn speed_steps_saturate_at_the_bounds() {
        assert_eq!(Speed::MAX.stepped(1), Speed::MAX);
        assert_eq!(Speed::MIN.stepped(-1), Speed::MIN);
        assert_eq!(Speed::NORMAL.stepped(5), speed(15));
        assert_eq!(Speed::NORMAL.stepped(-9), speed(1));
        assert_eq!(Speed::NORMAL.stepped(-10), Speed::MIN);
        assert_eq!(Speed::NORMAL.stepped(0), Speed::NORMAL);
    }

    #[test]
    fn speed_label_has_one_decimal_and_a_trailing_x() {
        assert_eq!(Speed::NORMAL.label(), "1.0x");
        assert_eq!(Speed::MIN.label(), "0.1x");
        assert_eq!(Speed::MAX.label(), "4.0x");
        assert_eq!(speed(15).label(), "1.5x");
        assert_eq!(Speed::NORMAL.decimal(), "1.0");
        assert_eq!(Speed::MIN.decimal(), "0.1");
    }

    #[test]
    fn speed_command_sets_the_property_with_the_decimal_value() {
        assert_eq!(
            speed(15).command().to_line(),
            "{\"command\":[\"set_property\",\"speed\",1.5]}\n"
        );
        assert_eq!(
            Speed::NORMAL.command().to_line(),
            "{\"command\":[\"set_property\",\"speed\",1.0]}\n"
        );
        assert_eq!(
            Speed::MIN.command().to_line(),
            "{\"command\":[\"set_property\",\"speed\",0.1]}\n"
        );
    }

    #[test]
    fn speed_launch_arg_is_omitted_at_normal_speed() {
        assert_eq!(Speed::NORMAL.launch_arg(), None);
        assert_eq!(speed(15).launch_arg(), Some("--speed=1.5".to_string()));
        assert_eq!(Speed::MIN.launch_arg(), Some("--speed=0.1".to_string()));
    }
}
