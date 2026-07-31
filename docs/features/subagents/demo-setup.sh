#!/usr/bin/env bash
set -euo pipefail

demo_dir=/tmp/yolop-subagents-mission-control
session_dir=/tmp/yolop-subagents-sessions

rm -rf "$demo_dir" "$session_dir"
mkdir -p "$demo_dir" "$session_dir"

node <<'NODE'
const fs = require("node:fs");
const path = require("node:path");

const root = "/tmp/yolop-subagents-mission-control";
const issues = [
  {
    group: "flight", id: "F1", name: "Orbit clock",
    brief: "periodSeconds(minutes) must convert minutes to seconds without rounding.",
    fix: "Multiply minutes by 60, not 1000.",
    source: `export function periodSeconds(minutes) {\n  return minutes * 1000;\n}\n`,
    test: `import test from "node:test";\nimport assert from "node:assert/strict";\nimport { periodSeconds } from "./subsystem.mjs";\ntest("converts orbit periods to seconds", () => {\n  assert.equal(periodSeconds(90), 5400);\n  assert.equal(periodSeconds(0.5), 30);\n});\n`,
  },
  {
    group: "flight", id: "F2", name: "Navigation heading",
    brief: "normalizeHeading(degrees) must return an equivalent heading in the range 0..359.",
    fix: "Use ((degrees % 360) + 360) % 360.",
    source: `export function normalizeHeading(degrees) {\n  return degrees % 360;\n}\n`,
    test: `import test from "node:test";\nimport assert from "node:assert/strict";\nimport { normalizeHeading } from "./subsystem.mjs";\ntest("normalizes signed headings", () => {\n  assert.equal(normalizeHeading(-10), 350);\n  assert.equal(normalizeHeading(725), 5);\n});\n`,
  },
  {
    group: "flight", id: "F3", name: "Guidance median",
    brief: "median(values) must use numeric ordering and average the two middle values for even input.",
    fix: "Sort numerically, then return the middle value or the average of the two middle values.",
    source: `export function median(values) {\n  const sorted = [...values].sort();\n  return sorted[Math.floor(sorted.length / 2)];\n}\n`,
    test: `import test from "node:test";\nimport assert from "node:assert/strict";\nimport { median } from "./subsystem.mjs";\ntest("computes numeric medians", () => {\n  assert.equal(median([2, 10, 3]), 3);\n  assert.equal(median([1, 9, 3, 5]), 4);\n});\n`,
  },
  {
    group: "flight", id: "F4", name: "Landing envelope",
    brief: "withinLandingEnvelope(speed, limit) treats the documented limit as inclusive and rejects negative speeds.",
    fix: "Return speed >= 0 && speed <= limit.",
    source: `export function withinLandingEnvelope(speed, limit) {\n  return speed < limit;\n}\n`,
    test: `import test from "node:test";\nimport assert from "node:assert/strict";\nimport { withinLandingEnvelope } from "./subsystem.mjs";\ntest("uses an inclusive non-negative landing envelope", () => {\n  assert.equal(withinLandingEnvelope(2, 2), true);\n  assert.equal(withinLandingEnvelope(-1, 2), false);\n});\n`,
  },
  {
    group: "vehicle", id: "V1", name: "Propulsion total",
    brief: "totalThrust(engines) must sum the thrust of every engine and return zero for an empty array.",
    fix: "Use 0 as the reducer's initial value.",
    source: `export function totalThrust(engines) {\n  return engines.reduce((sum, engine) => sum + engine.thrust, 1);\n}\n`,
    test: `import test from "node:test";\nimport assert from "node:assert/strict";\nimport { totalThrust } from "./subsystem.mjs";\ntest("totals engine thrust", () => {\n  assert.equal(totalThrust([{ thrust: 12 }, { thrust: 8 }]), 20);\n  assert.equal(totalThrust([]), 0);\n});\n`,
  },
  {
    group: "vehicle", id: "V2", name: "Power reserve",
    brief: "chargePercent(remaining, capacity) returns remaining capacity as a percentage, clamped to 0..100.",
    fix: "Compute (remaining / capacity) * 100 and clamp the result to 0..100.",
    source: `export function chargePercent(remaining, capacity) {\n  return Math.round((capacity / remaining) * 100);\n}\n`,
    test: `import test from "node:test";\nimport assert from "node:assert/strict";\nimport { chargePercent } from "./subsystem.mjs";\ntest("reports bounded charge percentage", () => {\n  assert.equal(chargePercent(40, 80), 50);\n  assert.equal(chargePercent(120, 80), 100);\n});\n`,
  },
  {
    group: "vehicle", id: "V3", name: "Thermal conversion",
    brief: "kelvinToCelsius(kelvin) uses the physical offset and preserves fractional precision.",
    fix: "Subtract 273.15 from kelvin.",
    source: `export function kelvinToCelsius(kelvin) {\n  return kelvin + 273.15;\n}\n`,
    test: `import test from "node:test";\nimport assert from "node:assert/strict";\nimport { kelvinToCelsius } from "./subsystem.mjs";\ntest("converts kelvin to celsius", () => {\n  assert.equal(kelvinToCelsius(273.15), 0);\n  assert.equal(kelvinToCelsius(300), 26.850000000000023);\n});\n`,
  },
  {
    group: "vehicle", id: "V4", name: "Fuel planner",
    brief: "requiredFuel(distance, rate, reserveFraction) applies the per-distance rate, then adds the fractional reserve.",
    fix: "Set base = distance * rate, then return base + base * reserveFraction to avoid floating-point drift.",
    source: `export function requiredFuel(distance, rate, reserveFraction) {\n  return distance / rate + reserveFraction;\n}\n`,
    test: `import test from "node:test";\nimport assert from "node:assert/strict";\nimport { requiredFuel } from "./subsystem.mjs";\ntest("plans fuel with reserve", () => {\n  assert.equal(requiredFuel(100, 2, 0.1), 220);\n  assert.equal(requiredFuel(0, 2, 0.1), 0);\n});\n`,
  },
  {
    group: "crew", id: "C1", name: "Oxygen limits",
    brief: "oxygenSafe(ppm, min, max) accepts both documented boundary values.",
    fix: "Use ppm >= min && ppm <= max.",
    source: `export function oxygenSafe(ppm, min, max) {\n  return ppm > min && ppm < max;\n}\n`,
    test: `import test from "node:test";\nimport assert from "node:assert/strict";\nimport { oxygenSafe } from "./subsystem.mjs";\ntest("accepts inclusive oxygen limits", () => {\n  assert.equal(oxygenSafe(195000, 195000, 235000), true);\n  assert.equal(oxygenSafe(235000, 195000, 235000), true);\n});\n`,
  },
  {
    group: "crew", id: "C2", name: "Cabin pressure",
    brief: "averagePressure(samples) computes the arithmetic mean and returns zero when there are no samples.",
    fix: "Return 0 for an empty array; otherwise divide the sum by samples.length.",
    source: `export function averagePressure(samples) {\n  return samples.reduce((sum, value) => sum + value, 0) / (samples.length - 1);\n}\n`,
    test: `import test from "node:test";\nimport assert from "node:assert/strict";\nimport { averagePressure } from "./subsystem.mjs";\ntest("averages cabin pressure", () => {\n  assert.equal(averagePressure([100, 102, 101]), 101);\n  assert.equal(averagePressure([]), 0);\n});\n`,
  },
  {
    group: "crew", id: "C3", name: "Payload manifest",
    brief: "manifestMass(items) multiplies each unit mass by its quantity before summing.",
    fix: "Add item.mass * item.quantity for each item.",
    source: `export function manifestMass(items) {\n  return items.reduce((sum, item) => sum + item.mass, 0);\n}\n`,
    test: `import test from "node:test";\nimport assert from "node:assert/strict";\nimport { manifestMass } from "./subsystem.mjs";\ntest("includes payload quantities", () => {\n  assert.equal(manifestMass([{ mass: 3, quantity: 4 }, { mass: 2, quantity: 1 }]), 14);\n});\n`,
  },
  {
    group: "crew", id: "C4", name: "Supply duration",
    brief: "wholeDaysRemaining(units, dailyUse) reports only complete days and rejects a zero daily rate.",
    fix: "Throw when dailyUse is zero; otherwise return Math.floor(units / dailyUse).",
    source: `export function wholeDaysRemaining(units, dailyUse) {\n  return Math.ceil(units / dailyUse);\n}\n`,
    test: `import test from "node:test";\nimport assert from "node:assert/strict";\nimport { wholeDaysRemaining } from "./subsystem.mjs";\ntest("counts complete supply days", () => {\n  assert.equal(wholeDaysRemaining(10, 3), 3);\n  assert.throws(() => wholeDaysRemaining(10, 0));\n});\n`,
  },
  {
    group: "ground", id: "G1", name: "Telemetry checksum",
    brief: "checksum(bytes) sums all byte values modulo 256.",
    fix: "Sum the bytes and return the sum modulo 256.",
    source: `export function checksum(bytes) {\n  return bytes.reduce((value, byte) => value ^ byte, 0);\n}\n`,
    test: `import test from "node:test";\nimport assert from "node:assert/strict";\nimport { checksum } from "./subsystem.mjs";\ntest("computes additive byte checksum", () => {\n  assert.equal(checksum([250, 10, 5]), 9);\n  assert.equal(checksum([]), 0);\n});\n`,
  },
  {
    group: "ground", id: "G2", name: "Radio frequency",
    brief: "megahertzToHertz(mhz) converts MHz to Hz exactly.",
    fix: "Multiply mhz by 1_000_000.",
    source: `export function megahertzToHertz(mhz) {\n  return mhz * 1000;\n}\n`,
    test: `import test from "node:test";\nimport assert from "node:assert/strict";\nimport { megahertzToHertz } from "./subsystem.mjs";\ntest("converts radio frequency units", () => {\n  assert.equal(megahertzToHertz(145.8), 145800000);\n});\n`,
  },
  {
    group: "ground", id: "G3", name: "Tracking order",
    brief: "sortObservations(rows) orders numeric timestamp strings ascending without mutating the input.",
    fix: "Sort a copied array with Number(a.timestamp) - Number(b.timestamp).",
    source: `export function sortObservations(rows) {\n  return rows.sort((a, b) => a.timestamp.localeCompare(b.timestamp));\n}\n`,
    test: `import test from "node:test";\nimport assert from "node:assert/strict";\nimport { sortObservations } from "./subsystem.mjs";\ntest("orders observations numerically and immutably", () => {\n  const rows = [{ timestamp: "10" }, { timestamp: "2" }, { timestamp: "1" }];\n  assert.deepEqual(sortObservations(rows).map((row) => row.timestamp), ["1", "2", "10"]);\n  assert.deepEqual(rows.map((row) => row.timestamp), ["10", "2", "1"]);\n});\n`,
  },
  {
    group: "ground", id: "G4", name: "Command parser",
    brief: "parseSequence(text) returns trimmed non-empty comma-separated commands.",
    fix: "Split on commas, trim each entry, and filter out empty entries.",
    source: `export function parseSequence(text) {\n  return text.split(",");\n}\n`,
    test: `import test from "node:test";\nimport assert from "node:assert/strict";\nimport { parseSequence } from "./subsystem.mjs";\ntest("parses clean command sequences", () => {\n  assert.deepEqual(parseSequence(" ARM, LAUNCH, ,SEPARATE "), ["ARM", "LAUNCH", "SEPARATE"]);\n  assert.deepEqual(parseSequence(""), []);\n});\n`,
  },
];

for (const issue of issues) {
  const slug = issue.name.toLowerCase().replaceAll(" ", "-");
  const dir = path.join(root, "issues", issue.group, `${issue.id.toLowerCase()}-${slug}`);
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, "README.md"), `# ${issue.id} — ${issue.name}\n\n${issue.brief}\n\nRequired correction: ${issue.fix}\n\nOwnership: edit only \`subsystem.mjs\` in this directory.\n\nVerify with:\n\n\`\`\`console\nnode --test ${path.relative(root, path.join(dir, "subsystem.test.mjs"))}\n\`\`\`\n`);
  fs.writeFileSync(path.join(dir, "subsystem.mjs"), issue.source);
  fs.writeFileSync(path.join(dir, "subsystem.test.mjs"), issue.test);
}

const issuePathsByGroup = ["flight", "vehicle", "crew", "ground"].map((group) => {
  const paths = issues
    .filter((issue) => issue.group === group)
    .map((issue) => {
      const slug = issue.name.toLowerCase().replaceAll(" ", "-");
      return `issues/${group}/${issue.id.toLowerCase()}-${slug}`;
    });
  const lead = `${group[0].toUpperCase()}${group.slice(1)} Lead`;
  return `- ${lead} must receive and delegate these exact paths: ${paths.join(", ")}.`;
}).join("\n");

fs.writeFileSync(path.join(root, "package.json"), JSON.stringify({
  name: "mission-control",
  private: true,
  type: "module",
  scripts: { test: "node --test" },
}, null, 2) + "\n");

fs.writeFileSync(path.join(root, "AGENTS.md"), `# Mission Control swarm rules

- The root coordinator must spawn exactly four background leads: Flight Lead, Vehicle Lead, Crew Lead, and Ground Lead. Then wait for all four tasks.
- Copy the matching exact-path line below into each lead's instructions; do not ask a lead to discover or infer issue names.
${issuePathsByGroup}
- A lead must immediately spawn exactly four background issue agents, one for each issue in its assigned group, then wait for all four tasks and summarize.
- A lead never spawns another lead. Its four child names start with the supplied issue IDs (F1..F4, V1..V4, C1..C4, or G1..G4), never with "Lead".
- An issue agent reads its README, edits only that issue's subsystem.mjs, and runs only its scoped test.
- Leads put one supplied exact issue directory in every child instruction; never rename, abbreviate, or substitute a path.
- Issue agents start by reading the exact README path from their instructions. They do not set a title/status or list workspace ancestors; they read, fix, test, and summarize.
- Every spawn_agent call uses exactly four fields: name, instructions, target = { type = subagent }, and mode = background. Never pass goal, lifetime, seed, blueprint, config, or schemas.
- Coordinators use list_tasks and wait_task after spawning. They do not end their turn while a child task is running.
- Leads coordinate only. The root runs the full suite and dashboard after every lead succeeds.
- Never use Git, the network, package installation, or shared-file edits.
`);

fs.writeFileSync(path.join(root, "dashboard.mjs"), [
  'import { readdirSync } from "node:fs";',
  'import { join } from "node:path";',
  'import { spawnSync } from "node:child_process";',
  '',
  'const groups = ["flight", "vehicle", "crew", "ground"];',
  'let passing = 0;',
  'console.log("\\n  MISSION CONTROL // READINESS BOARD\\n");',
  'for (const group of groups) {',
  '  const cells = [];',
  '  for (const issue of readdirSync(join("issues", group)).sort()) {',
  '    const test = join("issues", group, issue, "subsystem.test.mjs");',
  '    const ok = spawnSync(process.execPath, ["--test", test], { stdio: "ignore" }).status === 0;',
  '    passing += Number(ok);',
  '    const label = issue.split("-").slice(1, 3).join(" ").toUpperCase().padEnd(18);',
  '    cells.push(ok ? `\\x1b[32m✓ ${label}\\x1b[0m` : `\\x1b[31m✗ ${label}\\x1b[0m`);',
  '  }',
  '  console.log(`  ${group.toUpperCase().padEnd(8)} ${cells.join("  ")}`);',
  '}',
  'console.log(`\\n  ${passing === 16 ? "\\x1b[32mLAUNCH READY" : "\\x1b[31mHOLD"} — ${passing}/16 systems green\\x1b[0m\\n`);',
  'process.exitCode = passing === 16 ? 0 : 1;',
  '',
].join("\n"));
NODE
