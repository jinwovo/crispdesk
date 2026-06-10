// Unit tests for the pure Origin-allowlist helpers used to gate WS upgrades.
// Run with: npm test   (compiles to dist/ then `node --test`).
import { test } from "node:test";
import assert from "node:assert/strict";

import { isOriginAllowed, parseAllowedOrigins } from "../dist/pairing.js";

test("isOriginAllowed: empty allowList means no restriction (allow all)", () => {
  // Default behaviour when ALLOWED_ORIGINS is unset: everything is allowed.
  assert.equal(isOriginAllowed("https://evil.example", []), true);
  assert.equal(isOriginAllowed("http://localhost:8080", []), true);
  assert.equal(isOriginAllowed(undefined, []), true);
  assert.equal(isOriginAllowed(null, []), true);
  assert.equal(isOriginAllowed("", []), true);
});

test("isOriginAllowed: non-empty allowList permits only exact matches", () => {
  const allow = ["https://app.example.com", "http://localhost:8080"];

  assert.equal(isOriginAllowed("https://app.example.com", allow), true);
  assert.equal(isOriginAllowed("http://localhost:8080", allow), true);

  // Disallowed origins.
  assert.equal(isOriginAllowed("https://evil.example", allow), false);
  // Exact match required — scheme, host, and port all matter.
  assert.equal(isOriginAllowed("http://app.example.com", allow), false);
  assert.equal(isOriginAllowed("https://app.example.com:443", allow), false);
  assert.equal(isOriginAllowed("https://app.example.com/", allow), false);
  assert.equal(isOriginAllowed("http://localhost:9090", allow), false);
});

test("isOriginAllowed: missing/empty Origin is allowed even with an allowList", () => {
  // Native clients (the Rust host) send no Origin header. The allowlist targets
  // browser cross-origin abuse, so absent origins must not be blocked.
  const allow = ["https://app.example.com"];
  assert.equal(isOriginAllowed(undefined, allow), true);
  assert.equal(isOriginAllowed(null, allow), true);
  assert.equal(isOriginAllowed("", allow), true);
});

test("isOriginAllowed: exact string match, no case-folding or substring", () => {
  const allow = ["https://app.example.com"];
  // Case-sensitive: a different-case host is not a match.
  assert.equal(isOriginAllowed("https://APP.example.com", allow), false);
  // Substring/superstring are not matches.
  assert.equal(isOriginAllowed("https://app.example.com.evil", allow), false);
  assert.equal(isOriginAllowed("app.example.com", allow), false);
});

test("parseAllowedOrigins: splits, trims, drops empties", () => {
  assert.deepEqual(parseAllowedOrigins(undefined), []);
  assert.deepEqual(parseAllowedOrigins(""), []);
  assert.deepEqual(parseAllowedOrigins("   "), []);

  assert.deepEqual(
    parseAllowedOrigins("https://a.example, http://b.example"),
    ["https://a.example", "http://b.example"],
  );

  // Surrounding whitespace and trailing/empty segments are cleaned up.
  assert.deepEqual(
    parseAllowedOrigins("  https://a.example ,, ,http://b.example ,"),
    ["https://a.example", "http://b.example"],
  );

  // Single origin.
  assert.deepEqual(parseAllowedOrigins("https://only.example"), ["https://only.example"]);
});

test("parseAllowedOrigins + isOriginAllowed integration", () => {
  // End-to-end: an unset env var yields allow-all; a set one enforces the list.
  const noRestriction = parseAllowedOrigins(undefined);
  assert.equal(isOriginAllowed("https://anything.example", noRestriction), true);

  const restricted = parseAllowedOrigins("https://app.example.com");
  assert.equal(isOriginAllowed("https://app.example.com", restricted), true);
  assert.equal(isOriginAllowed("https://other.example.com", restricted), false);
});
