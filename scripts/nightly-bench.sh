#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

MODEL_NAME="$(sysctl -n hw.model 2>/dev/null || true)"
MEM_BYTES="$(sysctl -n hw.memsize 2>/dev/null || echo 0)"
if [[ "${MA_ALLOW_AIR_BENCH:-0}" != "1" ]]; then
  if [[ "$MODEL_NAME" == *"MacBookAir"* || "${MEM_BYTES:-0}" -lt 32000000000 ]]; then
    echo "[nightly-bench] refusing to run on lightweight host model=$MODEL_NAME mem_bytes=$MEM_BYTES" >&2
    echo "[nightly-bench] run this on a Pro-class lab host; set MA_ALLOW_AIR_BENCH=1 only for tiny smoke tests" >&2
    exit 12
  fi
fi

MA_BIN="${MA_BIN:-$REPO_ROOT/target/release/memra}"
QUERY_FILE="${MA_BENCH_QUERY_FILE:-$REPO_ROOT/scripts/benchmarks/v71-queries.tsv}"
PROJECT="${MA_BENCH_PROJECT:-memra}"
OUT_ROOT="${MA_BENCH_OUT_ROOT:-$REPO_ROOT/docs/dogfood-results}"
STAMP="${MA_BENCH_STAMP:-$(date +%Y-%m-%d-%H%M%S)}"
OUT_DIR="${MA_BENCH_OUT_DIR:-$OUT_ROOT/${STAMP}-bench}"
JUDGE_URL="${MA_BENCH_JUDGE_URL:-http://127.0.0.1:1234/v1/chat/completions}"
JUDGE_MODEL="${MA_BENCH_JUDGE_MODEL:-gemma-4-e4b-it}"
SEARCH_LIMIT="${MA_BENCH_SEARCH_LIMIT:-5}"
MIN_SCORE="${MA_BENCH_MIN_SCORE:-0.5}"
export MA_SCORE_NORMALIZE="${MA_SCORE_NORMALIZE:-z}"

if [[ ! -x "$MA_BIN" ]]; then
  echo "[nightly-bench] missing executable MA_BIN=$MA_BIN" >&2
  exit 2
fi
if [[ ! -f "$QUERY_FILE" ]]; then
  echo "[nightly-bench] missing query file $QUERY_FILE" >&2
  exit 2
fi

mkdir -p "$OUT_DIR"

ruby - "$MA_BIN" "$QUERY_FILE" "$PROJECT" "$OUT_DIR" "$JUDGE_URL" "$JUDGE_MODEL" "$SEARCH_LIMIT" "$MIN_SCORE" <<'RUBY'
require "json"
require "net/http"
require "open3"
require "time"
require "uri"

ma_bin, query_file, project, out_dir, judge_url, judge_model, search_limit, min_score = ARGV
started_at = Time.now.utc
raw_path = File.join(out_dir, "raw-results.jsonl")
summary_path = File.join(out_dir, "summary.json")
markdown_path = File.join(out_dir, "SUMMARY.md")

def percentile(values, pct)
  return nil if values.empty?
  sorted = values.sort
  index = ((pct / 100.0) * (sorted.length - 1)).ceil
  sorted[[index, sorted.length - 1].min]
end

def mean(values)
  return nil if values.empty?
  values.sum.to_f / values.length
end

def stddev(values)
  avg = mean(values)
  return nil if values.empty? || avg.nil?
  Math.sqrt(values.map { |value| (value - avg) ** 2 }.sum / values.length)
end

def parse_judge_json(raw)
  parsed = JSON.parse(raw)
  content = parsed.dig("choices", 0, "message", "content").to_s.strip
  json_text = content[/\{.*\}/m] || content
  JSON.parse(json_text)
rescue JSON::ParserError
  { "score" => nil, "reason" => "judge returned non-json: #{raw[0, 300]}" }
end

def judge_top1(judge_url, judge_model, query, expected, top_content, top_score)
  uri = URI(judge_url)
  prompt = <<~PROMPT
    Grade whether the retrieved top-1 Memra result answers the query.
    Return strict JSON only: {"score":0-5,"reason":"short reason"}.

    Scoring:
    5 = exact, directly useful fact for the query.
    4 = useful and mostly on target.
    3 = partially relevant but incomplete.
    2 = weakly related.
    1 = barely related.
    0 = wrong topic or no usable fact.

    Query: #{query}
    Expected focus: #{expected}
    Retrieval score: #{top_score}
    Top-1 content:
    #{top_content}
  PROMPT
  body = {
    model: judge_model,
    temperature: 0,
    max_tokens: 180,
    messages: [
      { role: "system", content: "You are a strict retrieval evaluator. Output JSON only." },
      { role: "user", content: prompt }
    ]
  }
  request = Net::HTTP::Post.new(uri)
  request["Content-Type"] = "application/json"
  request.body = JSON.dump(body)
  response = Net::HTTP.start(uri.host, uri.port, use_ssl: uri.scheme == "https", read_timeout: 120, open_timeout: 5) do |http|
    http.request(request)
  end
  raise "judge http #{response.code}: #{response.body[0, 300]}" unless response.is_a?(Net::HTTPSuccess)
  parsed = parse_judge_json(response.body)
  score = parsed["score"]
  score = score.to_i if score.is_a?(String) && score.match?(/\A\d+\z/)
  score = [[score.to_i, 0].max, 5].min if score.is_a?(Numeric)
  [score, parsed["reason"].to_s]
rescue => error
  [nil, "judge_error: #{error.message}"]
end

env = ENV.to_h
stdout, stderr, status = Open3.capture3(
  env,
  ma_bin,
  "bench",
  "retrieval",
  "--project",
  project,
  "--query-file",
  query_file,
  "--limit",
  search_limit.to_s,
  "--min-score",
  min_score.to_s,
  "--json"
)
unless status.success?
  warn "[nightly-bench] ma bench retrieval failed with exit #{status.exitstatus}"
  warn stderr unless stderr.strip.empty?
  warn stdout unless stdout.strip.empty?
  exit status.exitstatus || 1
end

bench = JSON.parse(stdout)
results = []
File.open(raw_path, "w") do |raw|
  bench.fetch("results").each do |entry|
    row = entry.merge(
      "search_status" => 0,
      "search_stderr" => ""
    )
    score, reason = judge_top1(
      judge_url,
      judge_model,
      row["query"],
      row["expected_focus"],
      row["top1_content"].to_s,
      row["top1_score"]
    )
    row["judge_score"] = score
    row["judge_reason"] = reason
    raw.puts(JSON.dump(row))
    raw.flush
    results << row
  end
end

judge_scores = results.map { |row| row["judge_score"] }.compact
top1_scores = results.map { |row| row["top1_score"] }.compact.map(&:to_f)
summary = {
  "started_at" => started_at.iso8601,
  "completed_at" => Time.now.utc.iso8601,
  "project" => project,
  "ma_bin" => ma_bin,
  "query_file" => query_file,
  "judge_url" => judge_url,
  "judge_model" => judge_model,
  "ma_score_normalize" => ENV["MA_SCORE_NORMALIZE"],
  "query_count" => bench["query_count"],
  "successful_searches" => bench["successful_searches"],
  "judge_count" => judge_scores.length,
  "top1_mean" => mean(judge_scores)&.round(4),
  "top1_p50" => percentile(judge_scores, 50),
  "top1_lt3" => judge_scores.count { |score| score < 3 },
  "top1_eq5" => judge_scores.count { |score| score == 5 },
  "top1_score_std" => stddev(top1_scores)&.round(6),
  "latency_ms" => bench["latency_ms"],
  "retrieval_bench" => bench,
  "raw_results" => raw_path
}
File.write(summary_path, JSON.pretty_generate(summary) + "\n")

lines = []
lines << "# Memra Retrieval Bench"
lines << ""
lines << "- Project: `#{project}`"
lines << "- Query count: #{summary["query_count"]}"
lines << "- Successful searches: #{summary["successful_searches"]}"
lines << "- Judge count: #{summary["judge_count"]}"
lines << "- top1_mean: #{summary["top1_mean"]}"
lines << "- top1_p50: #{summary["top1_p50"]}"
lines << "- top1<3: #{summary["top1_lt3"]}"
lines << "- top1=5: #{summary["top1_eq5"]}"
lines << "- top1 search-score std: #{summary["top1_score_std"]}"
lines << "- latency p50/p95/p99 ms: #{summary.dig("latency_ms", "p50")} / #{summary.dig("latency_ms", "p95")} / #{summary.dig("latency_ms", "p99")}"
lines << "- MA_SCORE_NORMALIZE: `#{summary["ma_score_normalize"]}`"
lines << ""
lines << "| # | Query | Judge | Top-1 score | Reason |"
lines << "|---|---|---:|---:|---|"
results.each do |row|
  reason = row["judge_reason"].to_s.gsub("\n", " ").gsub("|", "\\|")
  lines << "| #{row["index"]} | #{row["query"]} | #{row["judge_score"] || "ERR"} | #{row["top1_score"] || ""} | #{reason} |"
end
File.write(markdown_path, lines.join("\n") + "\n")

puts JSON.pretty_generate(summary)
RUBY

echo "[nightly-bench] wrote $OUT_DIR"
