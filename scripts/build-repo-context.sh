#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Build a reusable local context cache for the Agave workspace.

Usage:
  scripts/build-repo-context.sh [--output DIR] [--hotspot-since WINDOW] [--skip-hotspots]

Options:
  --output DIR         Write the cache to DIR instead of ./.repo-context
  --hotspot-since STR  Git history window for hotspot summaries (default: "180 days ago")
  --skip-hotspots      Skip git hotspot generation
  -h, --help           Show this help
EOF
}

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "Missing required command: $1" >&2
    exit 1
  fi
}

output_dir=""
skip_hotspots=0
hotspot_since="${HOTSPOT_SINCE:-180 days ago}"

while (($# > 0)); do
  case "$1" in
    --output)
      shift
      if (($# == 0)); then
        echo "--output requires a directory" >&2
        exit 1
      fi
      output_dir="$1"
      ;;
    --hotspot-since)
      shift
      if (($# == 0)); then
        echo "--hotspot-since requires a value" >&2
        exit 1
      fi
      hotspot_since="$1"
      ;;
    --skip-hotspots)
      skip_hotspots=1
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
  shift
done

require_cmd cargo
require_cmd jq
require_cmd git
require_cmd rg

repo_root="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
if [[ -z "$output_dir" ]]; then
  output_dir="$repo_root/.repo-context"
elif [[ "$output_dir" != /* ]]; then
  output_dir="$repo_root/$output_dir"
fi

tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/repo-context.XXXXXX")"
trap 'rm -rf "$tmp_dir"' EXIT

generated_at="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
git_head="$(git -C "$repo_root" rev-parse HEAD)"
git_branch="$(git -C "$repo_root" rev-parse --abbrev-ref HEAD)"

mkdir -p \
  "$output_dir" \
  "$output_dir/crates" \
  "$output_dir/focus" \
  "$output_dir/flows" \
  "$output_dir/tests" \
  "$output_dir/git"

cargo metadata --format-version 1 --no-deps > "$tmp_dir/metadata.json"

jq \
  --arg root "$repo_root" \
  --arg generated_at "$generated_at" \
  --arg git_head "$git_head" \
  --arg git_branch "$git_branch" \
  '
  def rel:
    ltrimstr($root + "/");

  .packages as $packages
  | [
      $packages[]
      | {
          name,
          version,
          description,
          manifest_path: (.manifest_path | rel),
          crate_dir: (.manifest_path | rel | sub("/Cargo.toml$"; "")),
          targets: [
            .targets[]
            | {
                name,
                kind,
                src_path: (.src_path | rel),
                test,
                doctest
              }
          ],
          feature_names: (.features | keys | sort),
          local_dependencies: ([.dependencies[] | select(.path != null and .kind != "dev") | .name] | unique | sort),
          local_dev_dependencies: ([.dependencies[] | select(.path != null and .kind == "dev") | .name] | unique | sort)
        }
    ] as $crates
  | {
      generated_at: $generated_at,
      git_head: $git_head,
      git_branch: $git_branch,
      workspace_root: ".",
      workspace_members: ($crates | length),
      lib_targets: ([ $crates[].targets[] | select(.kind | index("lib")) ] | length),
      bin_targets: ([ $crates[].targets[] | select(.kind | index("bin")) ] | length),
      crates: (
        $crates
        | map(
            . as $crate
            | . + {
                local_dependency_count: (.local_dependencies | length),
                local_reverse_dependencies: ([ $crates[] | select(.local_dependencies | index($crate.name)) | .name ] | sort),
                local_reverse_dependency_count: ([ $crates[] | select(.local_dependencies | index($crate.name)) | .name ] | length),
                bins: ([.targets[] | select(.kind | index("bin")) | .name] | unique | sort),
                libs: ([.targets[] | select(.kind | index("lib")) | .name] | unique | sort),
                integration_tests: ([.targets[] | select(.kind | index("test")) | .name] | unique | sort),
                bench_targets: ([.targets[] | select(.kind | index("bench")) | .name] | unique | sort)
              }
          )
      )
    }
  | . + {
      hubs_by_local_deps: (
        .crates
        | map({
            name,
            crate_dir,
            local_dependency_count,
            local_reverse_dependency_count
          })
        | sort_by(-.local_dependency_count, .name)
        | .[:20]
      ),
      hubs_by_reverse_deps: (
        .crates
        | map({
            name,
            crate_dir,
            local_dependency_count,
            local_reverse_dependency_count
          })
        | sort_by(-.local_reverse_dependency_count, .name)
        | .[:20]
      )
    }
  ' \
  "$tmp_dir/metadata.json" > "$output_dir/workspace.json"

jq \
  --arg generated_at "$generated_at" \
  --arg git_head "$git_head" \
  --arg git_branch "$git_branch" \
  --arg hotspot_since "$hotspot_since" \
  --arg output_dir "${output_dir#$repo_root/}" \
  --argjson skip_hotspots "$skip_hotspots" \
  '{
    generator: "scripts/build-repo-context.sh",
    version: 1,
    generated_at: $generated_at,
    git_head: $git_head,
    git_branch: $git_branch,
    output_dir: $output_dir,
    hotspot_since: $hotspot_since,
    skip_hotspots: $skip_hotspots,
    artifacts: [
      "workspace.json",
      "workspace.md",
      "crates/*.md",
      "focus/*.md",
      "focus/*.json",
      "flows/*.md",
      "tests/map.json",
      "git/hotspots.json",
      "git/hotspots.md",
      "README.md"
    ]
  }' \
  </dev/null > "$output_dir/manifest.json"

{
  printf '# Repo Context Cache\n\n'
  printf 'Generated by `%s`.\n\n' 'scripts/build-repo-context.sh'
  printf -- '- Generated: `%s`\n' "$generated_at"
  printf -- '- Git branch: `%s`\n' "$git_branch"
  printf -- '- Git head: `%s`\n' "$git_head"
  printf -- '- Workspace summary: [`workspace.md`](workspace.md)\n'
  printf -- '- Structured workspace data: [`workspace.json`](workspace.json)\n'
  printf -- '- Crate summaries: [`crates/`](crates)\n'
  printf -- '- Focused deep dives: [`focus/`](focus)\n'
  printf -- '- Reading maps: [`flows/`](flows)\n'
  printf -- '- Test command map: [`tests/map.json`](tests/map.json)\n'
  printf -- '- Git hotspots: [`git/hotspots.md`](git/hotspots.md)\n'
  printf '\n'
  printf 'Refresh with `%s` after manifest changes, large refactors, or when hotspot history gets stale.\n' './scripts/build-repo-context.sh'
} > "$output_dir/README.md"

{
  printf '# Agave Workspace Context\n\n'
  printf -- '- Generated: `%s`\n' "$generated_at"
  printf -- '- Git branch: `%s`\n' "$git_branch"
  printf -- '- Git head: `%s`\n' "$git_head"
  printf -- '- Workspace members: `%s`\n' "$(jq -r '.workspace_members' "$output_dir/workspace.json")"
  printf -- '- Lib targets: `%s`\n' "$(jq -r '.lib_targets' "$output_dir/workspace.json")"
  printf -- '- Bin targets: `%s`\n' "$(jq -r '.bin_targets' "$output_dir/workspace.json")"
  printf '\n'
  printf '## Start Here\n\n'
  printf -- '- `validator`: validator binary entrypoint and CLI wiring\n'
  printf -- '- `core`: runtime services, stages, replay/banking orchestration\n'
  printf -- '- `runtime`: bank, account state, execution coordination\n'
  printf -- '- `ledger`: blockstore, shreds, replay inputs\n'
  printf -- '- `rpc`: JSON-RPC surface and request routing\n'
  printf -- '- `gossip` and `turbine`: cluster data plane\n'
  printf -- '- `svm`: execution boundary\n'
  printf -- '- `accounts-db` and `snapshots`: persisted state and snapshot loading\n'
  printf '\n'
  printf '## Focused Docs\n\n'
  printf -- '- [`focus/accounts-db.md`](focus/accounts-db.md): module map, hot files, tests, benches, and reading order for `solana-accounts-db`\n'
  printf -- '- [`flows/accounts-db.md`](flows/accounts-db.md): task-oriented reading map for account storage, index, cache, clean, and shrink paths\n'
  printf '\n'
  printf '## Hub Crates By Local Dependencies\n\n'
  jq -r '.hubs_by_local_deps[:15][] | "- `\(.name)` (`\(.crate_dir)`): \(.local_dependency_count) local deps, \(.local_reverse_dependency_count) reverse deps"' "$output_dir/workspace.json"
  printf '\n'
  printf '## Most Depended-On Crates\n\n'
  jq -r '.hubs_by_reverse_deps[:15][] | "- `\(.name)` (`\(.crate_dir)`): \(.local_reverse_dependency_count) reverse deps, \(.local_dependency_count) local deps"' "$output_dir/workspace.json"
  printf '\n'
  printf '## Refresh\n\n'
  printf -- '- Run `%s`\n' './scripts/build-repo-context.sh'
  printf -- '- Refresh after `Cargo.toml`, `Cargo.lock`, or major crate-layout changes\n'
} > "$output_dir/workspace.md"

jq \
  '{
    generated_at,
    git_head,
    git_branch,
    packages: (
      .crates
      | sort_by(.name)
      | map(
          . as $crate
          | {
              name,
              crate_dir,
              manifest_path,
              bins,
              libs,
              integration_tests,
              bench_targets,
              local_dependency_count,
              local_reverse_dependency_count,
              commands: (
                ["cargo test -p " + $crate.name]
                + (if ($crate.libs | length) > 0 then ["cargo test -p " + $crate.name + " --lib"] else [] end)
                + ($crate.integration_tests | map("cargo test -p " + $crate.name + " --test " + .))
                + ($crate.bench_targets | map("cargo bench -p " + $crate.name + " --bench " + . + " --no-run"))
                + ["cargo check -p " + $crate.name]
              )
            }
        )
    )
  }' \
  "$output_dir/workspace.json" > "$output_dir/tests/map.json"

find "$output_dir/crates" -type f -name '*.md' -delete
jq -c '.crates[]' "$output_dir/workspace.json" | while IFS= read -r crate_json; do
  crate_name="$(jq -r '.name' <<<"$crate_json")"
  jq -r '
    def inline_list($items):
      if ($items | length) == 0 then
        "none"
      else
        "`" + ($items | join("`, `")) + "`"
      end;

    def target_list:
      if (.targets | length) == 0 then
        "none"
      else
        "`" + ([.targets[] | .name + " [" + (.kind | join(", ")) + "]"] | join("`, `")) + "`"
      end;

    "# \(.name)\n\n" +
    "- Path: `\(.crate_dir)`\n" +
    "- Manifest: `\(.manifest_path)`\n" +
    "- Description: " + (if .description then "`" + .description + "`" else "none" end) + "\n" +
    "- Targets: " + target_list + "\n" +
    "- Features: " + inline_list(.feature_names) + "\n" +
    "- Local deps (\(.local_dependency_count)): " + inline_list(.local_dependencies) + "\n" +
    "- Reverse deps (\(.local_reverse_dependency_count)): " + inline_list(.local_reverse_dependencies) + "\n" +
    "- Local dev deps: " + inline_list(.local_dev_dependencies) + "\n" +
    "- Integration tests: " + inline_list(.integration_tests) + "\n" +
    "- Benches: " + inline_list(.bench_targets) + "\n" +
    "- Suggested commands: `cargo check -p \(.name)`, `cargo test -p \(.name)`\n"
  ' <<<"$crate_json" > "$output_dir/crates/$crate_name.md"
done

find "$output_dir/focus" -type f \( -name '*.md' -o -name '*.json' \) -delete

{
  printf '# Reading Maps\n\n'
  printf 'These files are starting maps, not exhaustive architecture specs. Use them to choose the right crates before reading code.\n\n'
  printf -- '- [`validator-lifecycle.md`](validator-lifecycle.md): startup, CLI wiring, service ownership\n'
  printf -- '- [`transaction-lifecycle.md`](transaction-lifecycle.md): ingest, execution, persistence, propagation\n'
  printf -- '- [`networking.md`](networking.md): gossip, turbine, TPU/QUIC, connection plumbing\n'
  printf -- '- [`state-and-storage.md`](state-and-storage.md): accounts state, ledger, snapshots, long-term storage\n'
  printf -- '- [`accounts-db.md`](accounts-db.md): task-oriented map for the `solana-accounts-db` crate\n'
} > "$output_dir/flows/README.md"

{
  printf '# Validator Lifecycle\n\n'
  printf 'Start here when you need process startup, CLI wiring, or service ownership boundaries.\n\n'
  printf 'Typical reading order:\n'
  printf -- '- `validator`: binary entrypoint, CLI flags, startup wiring\n'
  printf -- '- `core`: validator services, replay/banking orchestration, stage ownership\n'
  printf -- '- `runtime`: bank/account state and execution coordination\n'
  printf -- '- `ledger`: blockstore, replay inputs, shred storage\n'
  printf -- '- `gossip`, `turbine`, `rpc`: networked subsystems started around the validator core\n'
} > "$output_dir/flows/validator-lifecycle.md"

{
  printf '# Transaction Lifecycle\n\n'
  printf 'Use this path for transaction ingest, execution, and persistence questions.\n\n'
  printf 'Typical crate path:\n'
  printf -- '- `client` or `rpc`: external submission surfaces\n'
  printf -- '- `send-transaction-service`: forwarding and retry behavior\n'
  printf -- '- `core`: transaction intake and banking-stage coordination\n'
  printf -- '- `runtime` and `svm`: bank execution, account access, and instruction processing\n'
  printf -- '- `ledger`: recording confirmed results and shred/blockstore output\n'
  printf -- '- `turbine` and `gossip`: propagation across the cluster\n'
} > "$output_dir/flows/transaction-lifecycle.md"

{
  printf '# Networking And Cluster Data Plane\n\n'
  printf 'Start here for gossip, TPU/QUIC, packet flow, and propagation behavior.\n\n'
  printf 'Key crates:\n'
  printf -- '- `gossip`: cluster membership and metadata propagation\n'
  printf -- '- `turbine`: block/shred propagation\n'
  printf -- '- `streamer`: packet and socket plumbing\n'
  printf -- '- `quic-client`, `udp-client`, `tpu-client`, `tpu-client-next`: client-side data paths\n'
  printf -- '- `connection-cache`: connection reuse and transport setup\n'
} > "$output_dir/flows/networking.md"

{
  printf '# State, Ledger, And Snapshots\n\n'
  printf 'Use this map for persistent state, replay data, and snapshot handling.\n\n'
  printf 'Key crates:\n'
  printf -- '- `accounts-db`: persisted account state and storage internals\n'
  printf -- '- `runtime`: bank state transitions and account loading\n'
  printf -- '- `ledger`: blockstore, shreds, and replay inputs\n'
  printf -- '- `snapshots`: snapshot packaging and restore paths\n'
  printf -- '- `storage-bigtable`: long-term external storage integrations\n'
} > "$output_dir/flows/state-and-storage.md"

{
  printf '# Accounts-Db Reading Map\n\n'
  printf 'Use this when most of your work is inside `solana-accounts-db`.\n\n'
  printf 'Recommended reading order:\n'
  printf -- '- `accounts-db/src/accounts_db.rs`: central orchestration for load/store, clean, shrink, roots, cache interactions, and background maintenance\n'
  printf -- '- `accounts-db/src/accounts.rs`: runtime-facing wrapper and higher-level account access flows built on top of `AccountsDb`\n'
  printf -- '- `accounts-db/src/accounts_index.rs` plus `accounts_index/*`: pubkey-to-slot index semantics, scan/upsert paths, in-memory index behavior, roots tracking, and secondary indexes\n'
  printf -- '- `accounts-db/src/account_storage.rs`, `account_storage_entry.rs`, `account_storage_reader.rs`: storage grouping, iteration, shrink orchestration, and append-vec readers\n'
  printf -- '- `accounts-db/src/append_vec.rs` and `accounts_file.rs`: on-disk account storage format and file-level IO behavior\n'
  printf -- '- `accounts-db/src/accounts_cache.rs` and `read_only_accounts_cache.rs`: write/read caching behavior and eviction policy\n'
  printf -- '- `accounts-db/src/ancient_append_vecs.rs`, `sorted_storages.rs`, `obsolete_accounts.rs`: shrink, compaction, and reclaim paths\n'
  printf -- '- `accounts-db/src/accounts_db/stats.rs` and `accounts_index/stats.rs`: metric structs and timing counters that explain runtime behavior during debugging\n'
  printf -- '- `accounts-db/src/accounts_db/tests.rs`, `accounts-db/tests/*.rs`, `accounts-db/benches/*.rs`: regression coverage, crate-local fixtures, and performance probes\n'
  printf '\n'
  printf 'Task guide:\n'
  printf -- '- Load/store bugs: start with `accounts_db.rs`, then `accounts.rs`, then cache/storage files\n'
  printf -- '- Index correctness or scan issues: start with `accounts_index.rs`, `in_mem_accounts_index.rs`, `account_map_entry.rs`, and `roots_tracker.rs`\n'
  printf -- '- Shrink/clean/reclaim behavior: start with `accounts_db.rs`, `ancient_append_vecs.rs`, `account_storage.rs`, `sorted_storages.rs`, and `obsolete_accounts.rs`\n'
  printf -- '- Storage-format or append-vec problems: start with `append_vec.rs`, `append_vec/meta.rs`, `accounts_file.rs`, and `account_storage_reader.rs`\n'
  printf -- '- Cache behavior: start with `accounts_cache.rs`, `read_only_accounts_cache.rs`, and the `PopulateReadCache` / `LoadHint` flows in `accounts_db.rs`\n'
  printf -- '- Geyser/update notifications: start with `accounts_update_notifier_interface.rs` and `accounts_db/geyser_plugin_utils.rs`\n'
} > "$output_dir/flows/accounts-db.md"

if ((skip_hotspots)); then
  jq \
    --arg generated_at "$generated_at" \
    --arg git_head "$git_head" \
    --arg git_branch "$git_branch" \
    --arg hotspot_since "$hotspot_since" \
    '{
      generated_at: $generated_at,
      git_head: $git_head,
      git_branch: $git_branch,
      since: $hotspot_since,
      skipped: true,
      top_files: [],
      top_crates: []
    }' \
    </dev/null > "$output_dir/git/hotspots.json"
else
  git -C "$repo_root" log --since="$hotspot_since" --format= --name-only -- . > "$tmp_dir/git-hotspots-raw.txt"

  awk '
    NF && $0 !~ /^\.repo-context\// {
      counts[$0]++
    }
    END {
      for (path in counts) {
        printf "%s\t%s\n", counts[path], path
      }
    }
  ' "$tmp_dir/git-hotspots-raw.txt" | sort -nr > "$tmp_dir/file-hotspots.tsv"

  jq -r '.crates | sort_by(-(.crate_dir | length), .crate_dir)[] | [.crate_dir, .name] | @tsv' "$output_dir/workspace.json" > "$tmp_dir/crate-prefixes.tsv"

  awk -F '\t' '
    FNR == NR {
      prefixes[++n] = $1
      names[n] = $2
      next
    }
    {
      count = $1
      file = $2
      owner = "workspace-root"
      for (i = 1; i <= n; i++) {
        prefix = prefixes[i]
        if (file == prefix || index(file, prefix "/") == 1) {
          owner = names[i]
          break
        }
      }
      counts[owner] += count
    }
    END {
      for (owner in counts) {
        printf "%s\t%s\n", counts[owner], owner
      }
    }
  ' "$tmp_dir/crate-prefixes.tsv" "$tmp_dir/file-hotspots.tsv" | sort -nr > "$tmp_dir/crate-hotspots.tsv"

  jq \
    -n \
    --arg generated_at "$generated_at" \
    --arg git_head "$git_head" \
    --arg git_branch "$git_branch" \
    --arg hotspot_since "$hotspot_since" \
    --rawfile file_counts_raw "$tmp_dir/file-hotspots.tsv" \
    --rawfile crate_counts_raw "$tmp_dir/crate-hotspots.tsv" \
    '
    def parse_tsv($raw; $field):
      $raw
      | split("\n")
      | map(select(length > 0))
      | map(split("\t"))
      | map({
          changes: (.[0] | tonumber),
          ($field): .[1]
        });

    {
      generated_at: $generated_at,
      git_head: $git_head,
      git_branch: $git_branch,
      since: $hotspot_since,
      skipped: false,
      top_files: (parse_tsv($file_counts_raw; "path") | .[:50]),
      top_crates: (parse_tsv($crate_counts_raw; "crate") | .[:30])
    }
    ' > "$output_dir/git/hotspots.json"
fi

{
  printf '# Git Hotspots\n\n'
  printf -- '- Generated: `%s`\n' "$generated_at"
  printf -- '- History window: `%s`\n' "$hotspot_since"
  printf '\n'
  printf 'Counts are file-touch frequency within the selected git history window, not line counts.\n\n'
  printf '## Top Crates\n\n'
  jq -r 'if (.top_crates | length) == 0 then "- none" else .top_crates[:15][] | "- `\(.crate)`: \(.changes) file touches" end' "$output_dir/git/hotspots.json"
  printf '\n'
  printf '## Top Files\n\n'
  jq -r 'if (.top_files | length) == 0 then "- none" else .top_files[:20][] | "- `\(.path)`: \(.changes) touches" end' "$output_dir/git/hotspots.json"
} > "$output_dir/git/hotspots.md"

jq '.crates[] | select(.name == "solana-accounts-db")' "$output_dir/workspace.json" > "$tmp_dir/accounts-db-crate.json"
git -C "$repo_root" ls-files accounts-db > "$tmp_dir/accounts-db-current-files.txt"

find "$repo_root/accounts-db/src" -type f -name '*.rs' | sed "s#^$repo_root/##" | sort > "$tmp_dir/accounts-db-src-files.txt"
find "$repo_root/accounts-db/tests" -type f -name '*.rs' 2>/dev/null | sed "s#^$repo_root/##" | sort > "$tmp_dir/accounts-db-test-files.txt"
find "$repo_root/accounts-db/benches" -type f -name '*.rs' 2>/dev/null | sed "s#^$repo_root/##" | sort > "$tmp_dir/accounts-db-bench-files.txt"

cat \
  "$tmp_dir/accounts-db-src-files.txt" \
  "$tmp_dir/accounts-db-test-files.txt" \
  "$tmp_dir/accounts-db-bench-files.txt" > "$tmp_dir/accounts-db-all-files.txt"

xargs -I {} wc -l "{}" < "$tmp_dir/accounts-db-all-files.txt" \
  | awk '$2 != "total" {printf "%s\t%s\n", $1, $2}' \
  | sort -nr > "$tmp_dir/accounts-db-line-counts.tsv"

awk '{sum += $1} END {print sum + 0}' "$tmp_dir/accounts-db-line-counts.tsv" > "$tmp_dir/accounts-db-total-lines.txt"

git -C "$repo_root" log --since="$hotspot_since" --format= --name-only -- accounts-db \
  | awk 'NF {counts[$0]++} END {for (path in counts) printf "%s\t%s\n", counts[path], path}' \
  | awk -F '\t' 'FNR == NR {keep[$0] = 1; next} keep[$2]' "$tmp_dir/accounts-db-current-files.txt" - \
  | sort -nr > "$tmp_dir/accounts-db-hotspots.tsv"

rg -n '^pub (struct|enum|trait|type|fn) ' "$repo_root/accounts-db/src" \
  | sed "s#^$repo_root/##" > "$tmp_dir/accounts-db-public-items.txt"

awk -F ':' '{counts[$1]++} END {for (path in counts) printf "%s\t%s\n", counts[path], path}' \
  "$tmp_dir/accounts-db-public-items.txt" | sort -nr > "$tmp_dir/accounts-db-public-item-counts.tsv"

rg -nH '^(pub )?mod ' "$repo_root/accounts-db/src/lib.rs" | sed "s#^$repo_root/##" > "$tmp_dir/accounts-db-lib-mods.txt"
rg -nH '^(pub )?mod ' "$repo_root/accounts-db/src/accounts_db.rs" | sed "s#^$repo_root/##" > "$tmp_dir/accounts-db-accounts-db-mods.txt"
rg -nH '^(pub )?mod ' "$repo_root/accounts-db/src/accounts_index.rs" | sed "s#^$repo_root/##" > "$tmp_dir/accounts-db-accounts-index-mods.txt"
rg -nH '^(pub )?mod ' "$repo_root/accounts-db/src/account_storage.rs" | sed "s#^$repo_root/##" > "$tmp_dir/accounts-db-account-storage-mods.txt"
rg -nH '^(pub )?mod ' "$repo_root/accounts-db/src/append_vec.rs" | sed "s#^$repo_root/##" > "$tmp_dir/accounts-db-append-vec-mods.txt"

jq \
  -n \
  --arg generated_at "$generated_at" \
  --arg git_head "$git_head" \
  --arg git_branch "$git_branch" \
  --arg hotspot_since "$hotspot_since" \
  --argjson source_file_count "$(wc -l < "$tmp_dir/accounts-db-src-files.txt" | tr -d ' ')" \
  --argjson test_file_count "$(wc -l < "$tmp_dir/accounts-db-test-files.txt" | tr -d ' ')" \
  --argjson bench_file_count "$(wc -l < "$tmp_dir/accounts-db-bench-files.txt" | tr -d ' ')" \
  --argjson total_lines "$(cat "$tmp_dir/accounts-db-total-lines.txt")" \
  --slurpfile crate "$tmp_dir/accounts-db-crate.json" \
  --rawfile line_counts_raw "$tmp_dir/accounts-db-line-counts.tsv" \
  --rawfile hotspots_raw "$tmp_dir/accounts-db-hotspots.tsv" \
  --rawfile public_items_raw "$tmp_dir/accounts-db-public-items.txt" \
  --rawfile public_item_counts_raw "$tmp_dir/accounts-db-public-item-counts.tsv" \
  --rawfile lib_mods_raw "$tmp_dir/accounts-db-lib-mods.txt" \
  --rawfile accounts_db_mods_raw "$tmp_dir/accounts-db-accounts-db-mods.txt" \
  --rawfile accounts_index_mods_raw "$tmp_dir/accounts-db-accounts-index-mods.txt" \
  --rawfile account_storage_mods_raw "$tmp_dir/accounts-db-account-storage-mods.txt" \
  --rawfile append_vec_mods_raw "$tmp_dir/accounts-db-append-vec-mods.txt" \
  '
  def parse_tsv($raw; $field1; $field2):
    $raw
    | split("\n")
    | map(select(length > 0))
    | map(split("\t"))
    | map({
        ($field1): (.[0] | tonumber),
        ($field2): .[1]
      });

  def parse_colon_records($raw):
    $raw
    | split("\n")
    | map(select(length > 0))
    | map(capture("(?<path>[^:]+):(?<line>[0-9]+):(?<text>.*)") | .line |= tonumber);

  $crate[0] as $crate
  | {
    generated_at: $generated_at,
    git_head: $git_head,
    git_branch: $git_branch,
    since: $hotspot_since,
    crate: $crate,
    code_size: {
      source_file_count: $source_file_count,
      test_file_count: $test_file_count,
      bench_file_count: $bench_file_count,
      total_rust_files: ($source_file_count + $test_file_count + $bench_file_count),
      total_lines: $total_lines,
      top_files_by_loc: (parse_tsv($line_counts_raw; "lines"; "path") | .[:20])
    },
    git_hotspots: {
      top_files: (parse_tsv($hotspots_raw; "changes"; "path") | .[:20])
    },
    module_surface: {
      lib_rs: parse_colon_records($lib_mods_raw),
      accounts_db_rs: parse_colon_records($accounts_db_mods_raw),
      accounts_index_rs: parse_colon_records($accounts_index_mods_raw),
      account_storage_rs: parse_colon_records($account_storage_mods_raw),
      append_vec_rs: parse_colon_records($append_vec_mods_raw)
    },
    public_surface: {
      total_items: (parse_colon_records($public_items_raw) | length),
      top_files_by_public_items: (parse_tsv($public_item_counts_raw; "items"; "path") | .[:20]),
      sample_items: (parse_colon_records($public_items_raw) | .[:80])
    },
    tests_and_benches: {
      integration_tests: $crate.integration_tests,
      bench_targets: $crate.bench_targets,
      suggested_commands: (
        ["cargo test -p " + $crate.name]
        + ["cargo test -p " + $crate.name + " --lib"]
        + ($crate.integration_tests | map("cargo test -p " + $crate.name + " --test " + .))
        + ($crate.bench_targets | map("cargo bench -p " + $crate.name + " --bench " + . + " --no-run"))
        + ["cargo check -p " + $crate.name]
      )
    }
  }
  ' > "$output_dir/focus/accounts-db.json"

{
  printf '# Accounts-Db Focus\n\n'
  printf -- '- Generated: `%s`\n' "$generated_at"
  printf -- '- Git branch: `%s`\n' "$git_branch"
  printf -- '- Git head: `%s`\n' "$git_head"
  printf -- '- Crate: `solana-accounts-db`\n'
  printf -- '- Path: `accounts-db`\n'
  printf -- '- Structured data: [`accounts-db.json`](accounts-db.json)\n'
  printf -- '- Reading map: [`../flows/accounts-db.md`](../flows/accounts-db.md)\n'
  printf '\n'
  printf '## Scope\n\n'
  printf -- '- Rust files: `%s` source, `%s` tests, `%s` benches, `%s` total lines\n' \
    "$(wc -l < "$tmp_dir/accounts-db-src-files.txt" | tr -d ' ')" \
    "$(wc -l < "$tmp_dir/accounts-db-test-files.txt" | tr -d ' ')" \
    "$(wc -l < "$tmp_dir/accounts-db-bench-files.txt" | tr -d ' ')" \
    "$(cat "$tmp_dir/accounts-db-total-lines.txt")"
  printf -- '- Features: `%s`\n' "$(jq -r '.crate.feature_names | join("`, `")' "$output_dir/focus/accounts-db.json")"
  printf -- '- Local deps: `%s`\n' "$(jq -r '.crate.local_dependencies | join("`, `")' "$output_dir/focus/accounts-db.json")"
  printf -- '- Reverse deps: `%s`\n' "$(jq -r '.crate.local_reverse_dependencies | join("`, `")' "$output_dir/focus/accounts-db.json")"
  printf '\n'
  printf '## Primary Files\n\n'
  jq -r '.code_size.top_files_by_loc[:12][] | "- `\(.path)`: \(.lines) lines"' "$output_dir/focus/accounts-db.json"
  printf '\n'
  printf '## Local Hotspots\n\n'
  jq -r 'if (.git_hotspots.top_files | length) == 0 then "- none" else .git_hotspots.top_files[:12][] | "- `\(.path)`: \(.changes) touches" end' "$output_dir/focus/accounts-db.json"
  printf '\n'
  printf '## Module Split\n\n'
  printf -- '- `lib.rs` exports: '
  jq -r '[.module_surface.lib_rs[] | .text | select(endswith(";")) | sub("^(pub )?mod "; "") | sub(";$"; "")] | unique | "`" + join("`, `") + "`"' "$output_dir/focus/accounts-db.json"
  printf -- '- `accounts_db.rs` submodules: '
  jq -r '[.module_surface.accounts_db_rs[] | .text | select(endswith(";")) | sub("^(pub )?mod "; "") | sub(";$"; "")] | unique | if length == 0 then "none" else "`" + join("`, `") + "`" end' "$output_dir/focus/accounts-db.json"
  printf -- '- `accounts_index.rs` submodules: '
  jq -r '[.module_surface.accounts_index_rs[] | .text | select(endswith(";")) | sub("^(pub )?mod "; "") | sub(";$"; "")] | unique | if length == 0 then "none" else "`" + join("`, `") + "`" end' "$output_dir/focus/accounts-db.json"
  printf -- '- `account_storage.rs` submodules: '
  jq -r '[.module_surface.account_storage_rs[] | .text | select(endswith(";")) | sub("^(pub )?mod "; "") | sub(";$"; "")] | unique | if length == 0 then "none" else "`" + join("`, `") + "`" end' "$output_dir/focus/accounts-db.json"
  printf -- '- `append_vec.rs` submodules: '
  jq -r '[.module_surface.append_vec_rs[] | .text | select(endswith(";")) | sub("^(pub )?mod "; "") | sub(";$"; "")] | unique | if length == 0 then "none" else "`" + join("`, `") + "`" end' "$output_dir/focus/accounts-db.json"
  printf '\n'
  printf '## Reading Order\n\n'
  printf -- '- Start with `accounts-db/src/accounts_db.rs` for end-to-end behavior and shared terminology\n'
  printf -- '- Then read `accounts-db/src/accounts.rs` for the wrapper layer used by the rest of the runtime\n'
  printf -- '- Use `accounts-db/src/accounts_index.rs` and `accounts-db/src/accounts_index/in_mem_accounts_index.rs` for index semantics and scan/update behavior\n'
  printf -- '- Use `append_vec.rs`, `accounts_file.rs`, and `account_storage*.rs` for storage format and file lifecycle\n'
  printf -- '- Use `accounts_cache.rs` and `read_only_accounts_cache.rs` for cache paths and eviction behavior\n'
  printf -- '- Use `ancient_append_vecs.rs`, `sorted_storages.rs`, and `obsolete_accounts.rs` for shrink and reclaim flows\n'
  printf -- '- Check `accounts-db/src/accounts_db/tests.rs` before changing tricky behavior; it is one of the largest local knowledge sources in the crate\n'
  printf '\n'
  printf '## Tests And Benches\n\n'
  jq -r '.tests_and_benches.suggested_commands[:12][] | "- `\(.)`"' "$output_dir/focus/accounts-db.json"
  printf '\n'
  printf '## Public Surface Hotspots\n\n'
  jq -r '.public_surface.top_files_by_public_items[:12][] | "- `\(.path)`: \(.items) public items"' "$output_dir/focus/accounts-db.json"
} > "$output_dir/focus/accounts-db.md"

printf 'Wrote repo context to %s\n' "$output_dir"
