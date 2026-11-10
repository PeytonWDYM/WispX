//! Fixed-viewbox chart geometry and number formatting for the Stats page.
use std::fmt::Write;

pub(super) struct Chart {
    pub line: String,
    pub area: String,
    pub top: u64,
    pub last_y: f32,
}

/// Groups digits without locale-dependent output or another dependency.
pub(super) fn number(value: u64) -> String {
    let digits = value.to_string();
    let mut result = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            result.push(',');
        }
        result.push(digit);
    }
    result
}

/// Monotone cubic interpolation keeps every segment within its endpoint values.
pub(super) fn chart(daily: &[u64; 30]) -> Chart {
    let max = (*daily.iter().max().unwrap()).max(1) as f64;
    let magnitude = 10_f64.powf(max.log10().floor());
    let top = [1.0, 2.0, 5.0, 10.0]
        .into_iter()
        .map(|step| step * magnitude)
        .find(|&step| step >= max)
        .unwrap();
    let y = daily.map(|words| 100.0 - words as f64 / top * 100.0);
    let slopes: [f64; 29] = std::array::from_fn(|i| y[i + 1] - y[i]);
    let tangents: [f64; 30] = std::array::from_fn(|i| match i {
        0 => slopes[0],
        29 => slopes[28],
        _ if slopes[i - 1] * slopes[i] <= 0.0 => 0.0,
        _ => 2.0 * slopes[i - 1] * slopes[i] / (slopes[i - 1] + slopes[i]),
    });
    let step = 300.0 / 29.0;
    let mut line = format!("M 0 {:.3}", y[0]);
    for i in 0..29 {
        let x = i as f64 * step;
        write!(
            line,
            " C {:.3} {:.3} {:.3} {:.3} {:.3} {:.3}",
            x + step / 3.0,
            y[i] + tangents[i] / 3.0,
            x + step * 2.0 / 3.0,
            y[i + 1] - tangents[i + 1] / 3.0,
            x + step,
            y[i + 1]
        )
        .unwrap();
    }
    let area = format!("{line} L 300 100 L 0 100 Z");
    Chart {
        line,
        area,
        top: top as u64,
        last_y: y[29] as f32,
    }
}
