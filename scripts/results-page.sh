#!/usr/bin/env bash
set -euo pipefail

usage="usage: scripts/results-page.sh <output directory> [directory of the latest run's results]"
[ $# -ge 1 ] && [ $# -le 2 ] || { echo "$usage" >&2; exit 2; }
out=$1
latest=${2:-}
root=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$out"
page="$out/index.html"

escape() {
  sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g'
}

markdown_table() {
  grep '^|' "$1" | grep -v '^|[-| :]*$' | escape | awk -F'|' '
    NR == 1 { printf "<table><thead><tr>"; for (i = 2; i < NF; i++) printf "<th>%s</th>", $i; print "</tr></thead><tbody>"; next }
    { printf "<tr>"; for (i = 2; i < NF; i++) printf "<td>%s</td>", $i; print "</tr>" }
    END { print "</tbody></table>" }'
}

ratio_chart() {
  awk '$3 ~ /_to_/ && $4 + 0 > 0 { print $1, $2 "." $3, $4 }' "$1" | sort -k3 -n | awk '
    BEGIN { height = 22; width = 760; scale = 220 }
    { system_[NR] = $1; name[NR] = $2; ratio[NR] = $3 }
    END {
      printf "<svg role=\"img\" aria-label=\"EggDB time as a multiple of the reference database, by benchmark\" viewBox=\"0 0 %d %d\" width=\"100%%\">\n", width, NR * height + 30
      printf "<line x1=\"%d\" y1=\"0\" x2=\"%d\" y2=\"%d\" class=\"parity\"/>\n", 440 + scale, 440 + scale, NR * height + 10
      printf "<text x=\"%d\" y=\"%d\" class=\"axis\">1.0 = as fast</text>\n", 440 + scale - 30, NR * height + 26
      for (i = 1; i <= NR; i++) {
        y = (i - 1) * height + 4
        bar = ratio[i] * scale; if (bar > 320) bar = 320
        class = ratio[i] <= 1.0 ? "ahead" : "behind"
        printf "<text x=\"0\" y=\"%d\" class=\"label\">%s %s</text>\n", y + 14, system_[i], name[i]
        printf "<rect x=\"440\" y=\"%d\" width=\"%.1f\" height=\"16\" class=\"%s\"><title>%s %s: %s</title></rect>\n", y, bar, class, system_[i], name[i], ratio[i]
        printf "<text x=\"%.1f\" y=\"%d\" class=\"value\">%s</text>\n", 444 + bar, y + 13, ratio[i]
      }
      print "</svg>"
    }'
}

conformance_summary() {
  grep '^|' "$1" | grep -v '^|[-| :]*$' | awk -F'|' 'NR > 1 { gsub(/^ +| +$/, "", $4); count[$4]++ } END { for (status in count) printf "<tr><td>%s</td><td>%d</td></tr>\n", status, count[status] }' | sort
}

target_table() {
  escape < "$1" | awk '{ printf "<tr><td>%s</td><td>%s</td><td>%s</td><td>%s</td></tr>\n", $1, $2, $3, $4 }'
}

{
  cat << 'HEAD'
<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>EggDB results</title>
<style>
:root { color-scheme: light dark; --ahead: #2a7d4f; --behind: #b4532a; --rule: #8884; }
body { font: 15px/1.5 system-ui, sans-serif; max-width: 1080px; margin: 2rem auto; padding: 0 1rem; }
table { border-collapse: collapse; width: 100%; margin: 0.5rem 0 1.5rem; }
th, td { text-align: left; padding: 0.25rem 0.5rem; border-bottom: 1px solid var(--rule); vertical-align: top; }
svg .label, svg .value, svg .axis { font: 12px system-ui, sans-serif; fill: currentColor; }
svg .ahead { fill: var(--ahead); } svg .behind { fill: var(--behind); }
svg .parity { stroke: currentColor; stroke-dasharray: 4 3; }
</style>
</head>
<body>
<h1>EggDB results</h1>
<p>Measured on GitHub's standard runners; each run's runner is described below. Lower is better in the chart: a bar left of the dashed line means EggDB took less time than the database it is compared with.</p>
HEAD
  echo "<p>Built from commit $(git -C "$root" rev-parse --short HEAD 2> /dev/null || echo unknown) on $(date -u +%Y-%m-%d).</p>"
  echo "<h2>Benchmarks against other databases</h2>"
  ratio_chart "$root/benchmarks/baseline.txt"
  echo "<h2>Footprint targets</h2><table><thead><tr><th>System</th><th>Profile</th><th>Measure</th><th>Target</th></tr></thead><tbody>"
  target_table "$root/benchmarks/footprint.txt"
  echo "</tbody></table>"
  if [ -f "$root/benchmarks/coverage.txt" ]; then
    echo "<h2>Test coverage</h2><table><thead><tr><th>Part</th><th>Measure</th><th>Target</th><th></th></tr></thead><tbody>"
    target_table "$root/benchmarks/coverage.txt"
    echo "</tbody></table>"
  fi
  echo "<h2>SQL conformance</h2><table><thead><tr><th>Status</th><th>Features</th></tr></thead><tbody>"
  conformance_summary "$root/docs/sql-conformance.md"
  echo "</tbody></table>"
  echo "<h2>Tools and drivers</h2>"
  markdown_table "$root/docs/compatibility.md"
  if [ -n "$latest" ]; then
    for matrix in "$latest"/*compatibility*.md "$latest"/*drivers*.md; do
      [ -f "$matrix" ] || continue
      echo "<h3>Latest farm run: $(basename "$matrix" | escape)</h3>"
      markdown_table "$matrix"
    done
    for runner in "$latest"/*runner*.md; do
      [ -f "$runner" ] || continue
      echo "<h2>Runner: $(basename "$runner" .md | escape)</h2>"
      markdown_table "$runner"
    done
  fi
  echo "</body></html>"
} > "$page"
echo "results page: $page"
