#!/usr/bin/env python3
"""Aggregate paired, order-interleaved A/B benchmark rounds into a drift-robust
comparison comment.

The old harness ran the whole feature suite, then the whole baseline suite, and
compared two single point estimates. Those two passes are minutes apart, so the
runner's slow drift (thermal throttling, frequency scaling, noisy neighbours)
lands entirely on the feature-vs-baseline axis and shows up as a ±2-5% phantom
delta — large enough to bury a real 2-4% optimization.

This script consumes the output of the new measurement strategy: R short rounds
where each round runs the feature binary and the baseline binary back-to-back
(A B A B ...). Because the two halves of a pair share the same local machine
state, the per-round paired Δ% cancels the common-mode drift. We then take the
mean of the R paired Δ% and a seeded bootstrap CI over them, and call a change
significant ONLY when that CI excludes zero — so a single lucky round can't flip
the verdict either.

The statistics (median / quantile / seeded bootstrap CI) are ported verbatim
from ARO's significance gate (aro/stats.py) so the two harnesses share one tested
implementation.

I/O is via environment variables:
  ROUNDS_DIR       directory holding the round files (default ".")
  OUT              output markdown path (default "comparison.md")
  FEATURE_SHA      feature commit (for the comment header link)
  BASELINE_SHA     baseline commit (for the comment header link)
  REPO_URL         repository html url (for the commit links)
  BOOTSTRAP_ITERS  bootstrap resamples (default 2000)
  AA_CHECK         "true" when feature_sha == baseline_sha (A/A self-check)

Round files are named "<phase>__<target>__r<NN>.txt" where phase is "feature"
or "baseline" and the body is criterion `--output-format bencher` output.
"""
from __future__ import annotations

import glob
import math
import os
import random
import re

# ---------------------------------------------------------------------------
# Statistics — ported verbatim from ARO (aro/stats.py). stdlib-only,
# deterministic given the seed so a re-run reproduces the same verdict.
# ---------------------------------------------------------------------------


def median(values) -> float:
    v = sorted(x for x in values if _finite(x))
    n = len(v)
    if n == 0:
        return math.nan
    if n % 2 == 1:
        return v[n // 2]
    return (v[n // 2 - 1] + v[n // 2]) / 2.0


def quantile(values, q: float) -> float:
    """Linear-interpolated quantile at q in [0,1]. NaNs dropped."""
    v = sorted(x for x in values if _finite(x))
    n = len(v)
    if n == 0:
        return math.nan
    if n == 1:
        return v[0]
    q = min(max(q, 0.0), 1.0)
    pos = q * (n - 1)
    lo = int(math.floor(pos))
    hi = int(math.ceil(pos))
    if hi >= n:
        hi = n - 1
    if lo == hi:
        return v[lo]
    frac = pos - lo
    return v[lo] + (v[hi] - v[lo]) * frac


def seed_for_metric(metric: str) -> int:
    """Stable 64-bit seed per metric (FNV-1a-ish) so each bootstrap is
    reproducible and independent of metric ordering."""
    h = 0xCBF29CE484222325
    for b in metric.encode("utf-8"):
        h ^= b
        h = (h * 0x00000100000001B3) & 0xFFFFFFFFFFFFFFFF
    return h ^ 0x9E3779B97F4A7C15


def bootstrap_ci(deltas_pct, iters: int = 2000, seed: int = 0):
    """~95% bootstrap CI (percent) on paired Δ% values; returns (low, high).

    Resamples with replacement `iters` times, takes each resample's mean, and
    returns the 2.5th / 97.5th percentiles of those means. Deterministic given
    `seed`. Empty input -> (0.0, 0.0)."""
    if not deltas_pct:
        return (0.0, 0.0)
    if iters == 0:
        m = sum(deltas_pct) / len(deltas_pct)
        return (m, m)
    rng = random.Random(seed)
    n = len(deltas_pct)
    means = []
    for _ in range(iters):
        s = 0.0
        for _ in range(n):
            s += deltas_pct[rng.randrange(n)]
        means.append(s / n)
    means.sort()
    return (quantile(means, 0.025), quantile(means, 0.975))


def _finite(x) -> bool:
    try:
        return not (math.isnan(x) or math.isinf(x))
    except TypeError:
        return False


# ---------------------------------------------------------------------------
# Parsing
# ---------------------------------------------------------------------------

# "test snailtracer/revm_pinned ... bench:    12345 ns/iter (+/- 678)"
_BENCH_LINE = re.compile(
    r"^test\s+(.+?)\s+\.\.\.\s+bench:\s+([\d,]+)\s+ns/iter\s+\(\+/-\s+([\d,]+)\)"
)
# "<phase>__<target>__r<NN>.txt"
_ROUND_FILE = re.compile(r"^(feature|baseline)__(.+)__r(\d+)\.txt$")

# A change must clear this many usable paired rounds before its bootstrap CI is
# allowed to claim significance — a CI over 1-2 points is not trustworthy.
MIN_PAIRS = 3

# Floor clamps, mirrored from ARO's calibrate_floors: never trust a floor below
# 0.5% (we don't claim to resolve sub-0.5% effects), and fall back to 2.0% when
# there aren't enough samples to estimate one.
FLOOR_MIN = 0.5
FLOOR_FALLBACK = 2.0


def parse_bencher(text: str) -> dict:
    out = {}
    for line in text.splitlines():
        m = _BENCH_LINE.match(line)
        if m:
            name = m.group(1).strip()
            ns = float(m.group(2).replace(",", ""))
            out[name] = ns
    return out


def load_rounds(rounds_dir: str):
    """Return {phase: {bench_name: {round: ns}}} for phase in feature/baseline."""
    data = {"feature": {}, "baseline": {}}
    for path in sorted(glob.glob(os.path.join(rounds_dir, "*.txt"))):
        fm = _ROUND_FILE.match(os.path.basename(path))
        if not fm:
            continue
        phase, _target, rnd = fm.group(1), fm.group(2), int(fm.group(3))
        try:
            with open(path, "r", encoding="utf-8") as fh:
                parsed = parse_bencher(fh.read())
        except OSError:
            continue
        for name, ns in parsed.items():
            data[phase].setdefault(name, {})[rnd] = ns
    return data


# ---------------------------------------------------------------------------
# Aggregation
# ---------------------------------------------------------------------------


def noise_floor(series_list) -> float:
    """A/A noise floor (percent) for one bench, computed from the round-to-round
    variation of repeated measurements of the SAME binary — so it costs no extra
    runs. Each phase's per-round values (feature_1..feature_R, baseline_1..R) are
    repeats of one binary; their consecutive relative |Δ%| estimate the machine's
    own jitter. Floor = 90th percentile of those magnitudes, clamped — exactly
    ARO's calibrate_floors, fed from in-band data instead of a separate A/A pass.

    `series_list` is a list of {round: ns} dicts (one per phase)."""
    mags = []
    for series in series_list:
        vals = [series[r] for r in sorted(series)]
        for a, b in zip(vals, vals[1:]):
            if _finite(a) and _finite(b) and a != 0.0:
                mags.append(abs((b - a) / a * 100.0))
    if len(mags) < 2:
        return FLOOR_FALLBACK
    q90 = quantile(mags, 0.90)
    return max(q90, FLOOR_MIN) if _finite(q90) else FLOOR_FALLBACK


class Comparison:
    __slots__ = (
        "name", "n_pairs", "mean_delta", "ci_low", "ci_high", "floor",
        "feature_ns", "baseline_ns", "significant", "noise_limited",
        "regressed", "improved",
    )

    def __init__(self, name, n_pairs, mean_delta, ci_low, ci_high, floor,
                 feature_ns, baseline_ns):
        self.name = name
        self.n_pairs = n_pairs
        self.mean_delta = mean_delta
        self.ci_low = ci_low
        self.ci_high = ci_high
        self.floor = floor
        self.feature_ns = feature_ns
        self.baseline_ns = baseline_ns
        # ARO's two-part gate: the bootstrap CI must exclude zero (the sign of
        # the effect is resolved) AND the effect must clear the machine's own
        # A/A noise floor (it's bigger than the jitter that produced the CI).
        # Either alone is a known false-positive source — drift fakes a clean CI,
        # and a tiny CI-excluding effect can still be pure measurement noise.
        excludes_zero = (ci_low > 0 and ci_high > 0) or (ci_low < 0 and ci_high < 0)
        enough = n_pairs >= MIN_PAIRS
        self.significant = excludes_zero and enough and abs(mean_delta) > floor
        # CI resolved a direction the floor just can't certify above its jitter.
        self.noise_limited = excludes_zero and enough and not self.significant
        self.regressed = self.significant and mean_delta > 0
        self.improved = self.significant and mean_delta < 0


def compare(data, iters: int):
    feature, baseline = data["feature"], data["baseline"]
    names = sorted(set(feature) & set(baseline))
    out = []
    for name in names:
        frounds, brounds = feature[name], baseline[name]
        deltas, fvals, bvals = [], [], []
        for rnd in sorted(set(frounds) & set(brounds)):
            f, b = frounds[rnd], brounds[rnd]
            if not _finite(f) or not _finite(b) or b == 0.0:
                continue
            deltas.append((f - b) / b * 100.0)
            fvals.append(f)
            bvals.append(b)
        if not deltas:
            continue
        mean_delta = sum(deltas) / len(deltas)
        ci_low, ci_high = bootstrap_ci(deltas, iters, seed_for_metric(name))
        floor = noise_floor([frounds, brounds])
        out.append(Comparison(name, len(deltas), mean_delta, ci_low, ci_high,
                              floor, median(fvals), median(bvals)))
    return out


# ---------------------------------------------------------------------------
# Baseline-gap section (HEAD only) — unchanged in spirit from the old harness:
# for every group that contains `revm_pinned`, show how each later row scales
# relative to it. Uses the feature (HEAD) median time per row.
# ---------------------------------------------------------------------------

BASELINE_ROW = "revm_pinned"
ROW_ORDER = [
    "revm_pinned", "revm_latest", "op_revm_pinned", "op_revm_latest",
    "equivalence", "mini_rex", "rex", "rex1", "rex2", "rex3", "rex4", "rex5",
]


def format_time(ns: float) -> str:
    if ns >= 1_000_000:
        return f"{ns / 1_000_000:.2f} ms"
    if ns >= 1_000:
        return f"{ns / 1_000:.2f} µs"
    return f"{ns:.0f} ns"


def baseline_gap_section(feature) -> str:
    # feature: {bench_name: {round: ns}} → median ns per row.
    feature_med = {name: median(list(r.values())) for name, r in feature.items()}
    groups = {}
    for full_name, ns in feature_med.items():
        slash = full_name.rfind("/")
        if slash <= 0:
            continue
        group, row = full_name[:slash], full_name[slash + 1:]
        groups.setdefault(group, {})[row] = ns

    out = ""
    for group in sorted(groups):
        rows = groups[group]
        base = rows.get(BASELINE_ROW)
        if base is None:
            continue
        ordered = [r for r in ROW_ORDER if rows.get(r) is not None]
        extras = sorted(r for r in rows if r not in ROW_ORDER)
        out += f"\n### `{group}`\n\n"
        out += "| spec | time | × vs `revm_pinned` |\n"
        out += "|------|------|--------------------|\n"
        for row in ordered + extras:
            ns = rows[row]
            marker = " (baseline)" if row == BASELINE_ROW else ""
            out += f"| `{row}` | {format_time(ns)} | {ns / base:.2f}×{marker} |\n"
    return out


# ---------------------------------------------------------------------------
# Rendering
# ---------------------------------------------------------------------------


def icon(c: Comparison) -> str:
    if c.improved:
        return ":rocket:"
    if c.regressed:
        return ":x:" if c.mean_delta > 15 else ":warning:"
    if c.noise_limited:
        return ":eyes:"  # directional, but below this machine's noise floor
    return ":white_circle:"


def ci_str(c: Comparison) -> str:
    return f"[{c.ci_low:+.1f}%, {c.ci_high:+.1f}%]"


def row_str(c: Comparison) -> str:
    b_us = f"{c.baseline_ns / 1000:.1f} µs"
    f_us = f"{c.feature_ns / 1000:.1f} µs"
    sign = "+" if c.mean_delta > 0 else ""
    return (f"| {c.name} | {b_us} | {f_us} | {sign}{c.mean_delta:.1f}% "
            f"| {ci_str(c)} | ±{c.floor:.1f}% | {icon(c)} |")


TABLE_HEADER = (
    "| Benchmark | Baseline | Feature | Δ (mean) | 95% CI | A/A floor | |\n"
    "|-----------|----------|---------|----------|--------|-----------|-|\n"
)

LEGEND = (
    "\n_:rocket: significant speedup · :x:/:warning: significant regression · "
    ":eyes: directional but below the A/A noise floor · :white_circle: within "
    "noise. **Significant = 95% bootstrap CI excludes 0 AND |Δ| > the A/A "
    "floor** (that bench's own round-to-round jitter)._\n"
)


def render(comparisons, feature_rounds, feature_sha, baseline_sha, repo_url,
           aa_check: bool) -> str:
    body = "## Criterion Benchmark Comparison\n\n"
    body += (
        f"> Comparing [`baseline`]({repo_url}/commit/{baseline_sha})"
        f" → [`feature`]({repo_url}/commit/{feature_sha})\n\n"
    )
    body += (
        "> **Paired, order-interleaved A/B.** Each round runs the feature and "
        "baseline binaries back-to-back on the same runner; the verdict is the "
        "mean of the per-round paired Δ% with a 95% bootstrap CI over the "
        "rounds. A change is flagged only when its **CI excludes 0 _and_ |Δ| "
        "clears the bench's A/A noise floor**, so minutes-apart machine drift no "
        "longer reads as a regression and a real sub-5% change is no longer "
        "buried in it.\n\n"
    )

    if aa_check:
        body += (
            "> :test_tube: **A/A self-check run** — feature and baseline are the "
            "**same commit**. Every row should report ≈0% with a CI that "
            "contains 0; that is the measured noise floor. Any row flagged "
            ":rocket:/:warning:/:x: here is a methodology bug.\n\n"
        )

    gap = baseline_gap_section(feature_rounds)
    if gap:
        body += "### Baseline gap (HEAD only — how far is each EVM layer from `revm_pinned`)\n"
        body += (
            "\n_Read the highest `rex*` row (currently `rex5`, the latest spec) "
            'for the "user-visible mega gap"; the earlier `rex*` rows are prior '
            "specs. The non-`rex` rows are diagnostic: they show which layer adds "
            "cost._\n"
        )
        body += gap
        body += "\n---\n\n### PR-base → PR-head regression check\n\n"

    if not comparisons:
        body += "_No comparable benchmarks found._\n"
        return body

    comparisons = sorted(comparisons, key=lambda c: c.name)
    regressions = sum(1 for c in comparisons if c.regressed)
    improvements = sum(1 for c in comparisons if c.improved)
    noise_limited = sum(1 for c in comparisons if c.noise_limited)
    significant = [c for c in comparisons if c.significant]
    rows = [row_str(c) for c in comparisons]

    if len(rows) > 20 and 0 < len(significant) < len(rows):
        body += (
            f"<details><summary>{len(rows)} benchmarks total, "
            f"{len(significant)} significant</summary>\n\n"
        )
        body += TABLE_HEADER + "\n".join(rows) + "\n\n</details>\n\n"
        body += "**Significant changes:**\n\n"
        body += TABLE_HEADER + "\n".join(row_str(c) for c in significant) + "\n"
    else:
        body += TABLE_HEADER + "\n".join(rows) + "\n"

    body += LEGEND
    n_rounds = max((c.n_pairs for c in comparisons), default=0)
    within = len(rows) - len(significant) - noise_limited
    body += (
        f"\n**{len(rows)} benchmarks** (paired over up to {n_rounds} rounds): "
        f"{regressions} significant regressions, {improvements} significant "
        f"improvements, {noise_limited} directional-but-noise-limited, "
        f"{within} within noise\n"
    )
    return body


def main():
    rounds_dir = os.environ.get("ROUNDS_DIR", ".")
    out_path = os.environ.get("OUT", "comparison.md")
    feature_sha = os.environ.get("FEATURE_SHA", "")
    baseline_sha = os.environ.get("BASELINE_SHA", "")
    repo_url = os.environ.get("REPO_URL", "")
    iters = int(os.environ.get("BOOTSTRAP_ITERS", "2000"))
    aa_check = os.environ.get("AA_CHECK", "").lower() == "true"

    data = load_rounds(rounds_dir)
    comparisons = compare(data, iters)
    body = render(comparisons, data["feature"], feature_sha, baseline_sha,
                  repo_url, aa_check)

    with open(out_path, "w", encoding="utf-8") as fh:
        fh.write(body)
    print(body)


if __name__ == "__main__":
    main()
