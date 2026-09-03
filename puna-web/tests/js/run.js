// The JavaScript half of this crate's tests.
//
// `puna-web/static/` holds ten scripts and **nothing in the Rust build parses one of them**, so
// until this existed a syntax error shipped and the page silently lost its behavior. Worse, the
// logic with real sequencing in it (the journal's follow loop) could only be checked by hand.
//
// Run it directly, or let CI's `js` job do it:
//
//     node puna-web/tests/js/run.js
//
// ## Why it lives here and NOT under `static/`
//
// `static/` is embedded into the binary by `rust-embed` and hashed by `build.rs` into
// `STATIC_VERSION`. A test file there would be **served** at `/static/tests/...` and would move the
// cache-busting version of every other asset. So the tests sit beside the Rust ones instead, in a
// directory cargo ignores: it discovers `tests/*.rs` and `tests/*/main.rs`, and this is neither.
//
// ## How a test is written
//
// A file named `*.test.js` beside this one, exporting `run(t)` and calling `t.check(label, ok)` as
// many times as it likes. No dependencies, no framework, no `package.json`: this repository's whole
// point is having no npm, and a test runner is not the thing to break that for.
"use strict";

const fs = require("fs");
const path = require("path");
const vm = require("vm");

const here = __dirname;
const staticDir = path.join(here, "..", "..", "static");

let failures = 0;
let checks = 0;

function check(label, ok) {
  checks++;
  if (!ok) failures++;
  console.log((ok ? "  ok   " : "  FAIL ") + label);
}

// --- every script compiles ----------------------------------------------------------------------
//
// `vm.Script` compiles without running, which is what `node --check` does. Done here rather than in
// the CI script so the two ways of running this cannot check different files.
console.log("compiling static/*.js");
const scripts = fs
  .readdirSync(staticDir)
  .filter((name) => name.endsWith(".js"))
  .sort();

for (const name of scripts) {
  const file = path.join(staticDir, name);
  let ok = true;
  try {
    new vm.Script(fs.readFileSync(file, "utf8"), { filename: name });
  } catch (e) {
    ok = false;
    console.log("       " + e.message);
  }
  check(name, ok);
}

// **A runner that finds nothing passes**, and this one is a directory listing. Thirteen scripts
// exist today; the floor is what says it is still reading the directory it thinks it is.
check("at least ten scripts were compiled", scripts.length >= 10);

// --- and every test file runs -------------------------------------------------------------------
const tests = fs
  .readdirSync(here)
  .filter((name) => name.endsWith(".test.js"))
  .sort();

for (const name of tests) {
  console.log(name);
  require(path.join(here, name)).run({ check });
}

check("at least one test file ran", tests.length >= 1);

console.log(
  (failures ? "FAILED" : "ok") + ": " + (checks - failures) + "/" + checks + " checks"
);
process.exit(failures ? 1 : 0);
