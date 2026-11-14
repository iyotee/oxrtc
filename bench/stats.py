"""Means and 95 % confidence intervals across runs (Student's t, two-sided).

The bench never summarizes a median: the live bench's lag was quantized in
70 ms steps, and a median of quantized values hides any difference smaller
than the step. Means of per-run values, with the interval, are what is compared;
a difference inside the interval is "no difference".
"""

from __future__ import annotations

import math
from collections.abc import Sequence
from dataclasses import dataclass

#: Two-sided 97.5 % quantiles of Student's t by degrees of freedom.
_T975 = {
    1: 12.706, 2: 4.303, 3: 3.182, 4: 2.776, 5: 2.571, 6: 2.447, 7: 2.365, 8: 2.306,
    9: 2.262, 10: 2.228, 11: 2.201, 12: 2.179, 13: 2.160, 14: 2.145, 15: 2.131,
    16: 2.120, 17: 2.110, 18: 2.101, 19: 2.093, 20: 2.086, 21: 2.080, 22: 2.074,
    23: 2.069, 24: 2.064, 25: 2.060, 26: 2.056, 27: 2.052, 28: 2.048, 29: 2.045,
    30: 2.042, 40: 2.021, 60: 2.000, 120: 1.980,
}


def t975(df: int) -> float:
    """Student's t quantile for a 95 % two-sided interval; conservative between table rows."""
    if df < 1:
        raise ValueError("degrees of freedom must be >= 1")
    if df in _T975:
        return _T975[df]
    below = [k for k in _T975 if k < df]
    if df > 120:
        return 1.960
    return _T975[max(below)]


@dataclass(frozen=True)
class Interval:
    n: int
    mean: float
    half_width: float | None  # None when n < 2: no interval from one run

    @property
    def low(self) -> float | None:
        return None if self.half_width is None else self.mean - self.half_width

    @property
    def high(self) -> float | None:
        return None if self.half_width is None else self.mean + self.half_width

    def overlaps(self, other: "Interval") -> bool:
        """True when the two intervals overlap (or either has none): "no difference"."""
        if self.half_width is None or other.half_width is None:
            return True
        return self.low <= other.high and other.low <= self.high  # type: ignore[operator]


def mean_ci(values: Sequence[float]) -> Interval | None:
    """Mean and 95 % CI half-width of *values*; None for an empty list."""
    clean = [float(v) for v in values if v is not None and not math.isnan(float(v))]
    n = len(clean)
    if n == 0:
        return None
    mean = sum(clean) / n
    if n < 2:
        return Interval(n, mean, None)
    var = sum((v - mean) ** 2 for v in clean) / (n - 1)
    return Interval(n, mean, t975(n - 1) * math.sqrt(var / n))


def fmt(interval: Interval | None, digits: int = 1) -> str:
    """`mean ± half` (n), or `mean (n=1, no CI)`."""
    if interval is None:
        return "-"
    if interval.half_width is None:
        return f"{interval.mean:.{digits}f} (n=1, no CI)"
    return f"{interval.mean:.{digits}f} ± {interval.half_width:.{digits}f} (n={interval.n})"
