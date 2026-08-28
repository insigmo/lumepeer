#!/usr/bin/env node
// Keeps the webview's `invoke` calls and the Rust `#[tauri::command]`
// signatures describing the same arguments.
//
// The two sides of the IPC boundary agree by convention and nothing else.
// TypeScript type-checks the *webview's* view of a command — the
// `ToolbarCommands` interface and friends — which is a description of what the
// webview believes, not of what Rust declares, so a call can be wrong in every
// way that matters and still compile, still pass the unit tests (which inject
// fakes shaped like the TypeScript interface), and still fail at runtime with
// a rejected promise the caller swallows.
//
// That is not hypothetical. Three commands shipped broken at once:
//
//   monitors_list   sent `{ args: { peer } }`; the command takes `peer`
//   monitor_select  sent `monitorId`;          the struct field is `monitor_id`
//   file_abort      sent `transferId`;         the struct field is `transfer_id`
//
// Tauri snake_cases the *parameter* names of a command, but the fields of a
// struct it hands to serde are matched verbatim. So the picker never listed a
// monitor, choosing one never did anything, and cancelling a file transfer was
// a button that returned an error into an empty `.catch`.
//
// This walks both sides and compares them. It is deliberately a text scan
// rather than a type generator: generating the bindings would solve the
// problem outright but is a change to how the whole frontend calls Rust
// (docs/bugs/09-monitors-and-ipc.md, task 3, option B), and this catches the
// one class of defect that actually occurred, in a file that has no
// dependencies and needs no build step.
//
// What it cannot see: a call whose command name is built at runtime. Every
// call site that names its command with a string literal is checked, including
// the ones that reach `invoke` through a helper, because the literal is what
// is matched rather than the callee.

import { readFileSync, readdirSync, statSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const COMMANDS_RS = join(root, "apps", "desktop", "src-tauri", "src", "commands.rs");
const WEBVIEW_SRC = join(root, "apps", "desktop", "src");

/** Parameters every command may declare and the webview never supplies. */
const INJECTED_PARAM_TYPES = [/^Window$/, /^tauri::AppHandle$/, /^tauri::State</];

const problems = [];

function fail(where, message) {
  problems.push(`${where}: ${message}`);
}

/** Splits `text` on `,` at bracket depth zero. */
function splitTopLevel(text) {
  const parts = [];
  let depth = 0;
  let start = 0;
  for (let i = 0; i < text.length; i += 1) {
    const c = text[i];
    if (c === "(" || c === "[" || c === "{" || c === "<") depth += 1;
    else if (c === ")" || c === "]" || c === "}" || c === ">") depth -= 1;
    else if (c === "," && depth === 0) {
      parts.push(text.slice(start, i));
      start = i + 1;
    }
  }
  parts.push(text.slice(start));
  return parts.map((part) => part.trim()).filter((part) => part.length > 0);
}

/** The balanced span starting at the opening bracket `open` at `from`. */
function balancedSpan(text, from, open, close) {
  let depth = 0;
  for (let i = from; i < text.length; i += 1) {
    if (text[i] === open) depth += 1;
    else if (text[i] === close) {
      depth -= 1;
      if (depth === 0) return text.slice(from + 1, i);
    }
  }
  return null;
}

/** Rust argument structs: name -> [{ field, optional }]. */
function parseArgStructs(rust) {
  const structs = new Map();
  const re = /pub struct (\w+) \{/g;
  for (let m = re.exec(rust); m !== null; m = re.exec(rust)) {
    const name = m[1];
    const braceAt = rust.indexOf("{", m.index);
    const body = balancedSpan(rust, braceAt, "{", "}");
    if (body === null) continue;
    // `rename_all` would change every field name on the wire. This boundary is
    // snake_case on purpose, so its appearance is the defect rather than
    // something to model.
    const derives = rust.slice(Math.max(0, m.index - 300), m.index);
    if (/#\[serde\(rename_all[\s\S]*$/.test(derives.slice(derives.lastIndexOf("#[derive")))) {
      fail(`commands.rs ${name}`, "carries serde(rename_all); this boundary is snake_case");
    }
    const fields = [];
    for (const line of body.split("\n")) {
      const f = /^\s*pub (\w+): (.+?),?\s*$/.exec(line);
      if (!f) continue;
      fields.push({ field: f[1], optional: f[2].startsWith("Option<") });
    }
    structs.set(name, fields);
  }
  return structs;
}

/**
 * Rust commands: name -> the shape the webview has to send.
 *
 * `{ kind: "none" }`                  takes nothing from the webview
 * `{ kind: "params", keys }`          top-level keys, each a command parameter
 * `{ kind: "struct", param, fields }` one struct parameter, usually `args`
 */
function parseCommands(rust, structs) {
  const commands = new Map();
  const re = /#\[tauri::command\]\s*pub (?:async )?fn (\w+)\s*\(/g;
  for (let m = re.exec(rust); m !== null; m = re.exec(rust)) {
    const name = m[1];
    const params = balancedSpan(rust, m.index + m[0].length - 1, "(", ")");
    if (params === null) continue;
    const supplied = splitTopLevel(params)
      .map((param) => {
        const p = /^(\w+)\s*:\s*([\s\S]+)$/.exec(param);
        return p ? { name: p[1], type: p[2].trim() } : null;
      })
      .filter((param) => param !== null)
      .filter((param) => !INJECTED_PARAM_TYPES.some((pattern) => pattern.test(param.type)));

    if (supplied.length === 0) {
      commands.set(name, { kind: "none" });
      continue;
    }
    if (supplied.length === 1 && structs.has(supplied[0].type)) {
      commands.set(name, {
        kind: "struct",
        param: supplied[0].name,
        fields: structs.get(supplied[0].type),
      });
      continue;
    }
    commands.set(name, {
      kind: "params",
      keys: supplied.map((param) => ({
        field: param.name,
        optional: param.type.startsWith("Option<"),
      })),
    });
  }
  return commands;
}

/** Keys of one object literal, or null when it is not one this can read. */
function objectKeys(text) {
  const keys = [];
  for (const part of splitTopLevel(text)) {
    const withValue = /^(['"]?)([A-Za-z_$][\w$]*)\1\s*:/.exec(part);
    if (withValue) {
      keys.push(withValue[2]);
      continue;
    }
    const shorthand = /^([A-Za-z_$][\w$]*)$/.exec(part);
    if (shorthand) {
      keys.push(shorthand[1]);
      continue;
    }
    return null; // a spread, a computed key: the keys are not written here
  }
  return keys;
}

function tsFiles(dir) {
  const out = [];
  for (const entry of readdirSync(dir)) {
    const path = join(dir, entry);
    if (statSync(path).isDirectory()) {
      out.push(...tsFiles(path));
    } else if (entry.endsWith(".ts") && !entry.endsWith(".test.ts") && !entry.endsWith(".d.ts")) {
      out.push(path);
    }
  }
  return out;
}

function checkKeys(where, sent, expected, what) {
  const wanted = new Set(expected.map((entry) => entry.field));
  for (const key of sent) {
    if (!wanted.has(key)) {
      fail(where, `sends ${what} \`${key}\`, which the Rust side does not take`);
    }
  }
  for (const entry of expected) {
    if (!entry.optional && !sent.includes(entry.field)) {
      fail(where, `never sends ${what} \`${entry.field}\`, which the Rust side requires`);
    }
  }
}

const rust = readFileSync(COMMANDS_RS, "utf8");
const commands = parseCommands(rust, parseArgStructs(rust));
if (commands.size === 0) {
  console.error("ipc-contract: found no #[tauri::command] in commands.rs — the parser is stale");
  process.exit(2);
}

let checked = 0;
for (const file of tsFiles(WEBVIEW_SRC)) {
  const source = readFileSync(file, "utf8");
  const rel = file.slice(root.length + 1).replaceAll("\\", "/");
  // Every string literal that names a command, wherever it is written: some
  // call sites hand the name to a helper that invokes it (`invite-view.ts`),
  // and those are exactly as able to get the shape wrong.
  const re = /['"]([a-z][a-z0-9_]*)['"]/g;
  for (let m = re.exec(source); m !== null; m = re.exec(source)) {
    const shape = commands.get(m[1]);
    if (shape === undefined) continue;
    const line = source.slice(0, m.index).split("\n").length;
    const where = `${rel}:${line} ${m[1]}`;
    const rest = source.slice(m.index + m[0].length);
    const withObject = /^\s*,\s*\{/.exec(rest);

    if (/^\s*\)/.test(rest)) {
      checked += 1;
      if (shape.kind !== "none") {
        fail(where, "is called with no arguments, but the Rust side takes some");
      }
      continue;
    }
    if (!withObject) continue; // not a call site, or one built at runtime

    const braceAt = m.index + m[0].length + withObject[0].length - 1;
    const body = balancedSpan(source, braceAt, "{", "}");
    if (body === null) continue;
    const top = objectKeys(body);
    if (top === null) continue;
    checked += 1;

    if (shape.kind === "none") {
      fail(where, `is called with { ${top.join(", ")} }, but the Rust side takes nothing`);
      continue;
    }
    if (shape.kind === "params") {
      checkKeys(where, top, shape.keys, "parameter");
      continue;
    }
    // One struct parameter: the object has exactly that key, and its own
    // object carries the struct's fields verbatim — Tauri renames parameters,
    // never struct fields.
    if (top.length !== 1 || top[0] !== shape.param) {
      fail(
        where,
        `sends { ${top.join(", ")} }, but the Rust side takes one \`${shape.param}\` struct`,
      );
      continue;
    }
    const nestedAt = new RegExp(`\\b${shape.param}\\s*:\\s*\\{`).exec(body);
    if (!nestedAt) {
      fail(where, `passes \`${shape.param}\` as something other than an object literal`);
      continue;
    }
    const nested = balancedSpan(body, nestedAt.index + nestedAt[0].length - 1, "{", "}");
    const fields = nested === null ? null : objectKeys(nested);
    if (fields === null) continue;
    checkKeys(where, fields, shape.fields, "field");
  }
}

if (problems.length > 0) {
  console.error("ipc-contract: the webview and the Rust commands disagree\n");
  for (const problem of problems) console.error(`  ${problem}`);
  console.error(
    "\nThe fix is on the TypeScript side: Tauri snake_cases command parameters," +
      "\nbut struct fields are matched verbatim, and this boundary is snake_case.",
  );
  process.exit(1);
}

console.log(`ipc-contract: ${checked} call sites agree with ${commands.size} commands`);
