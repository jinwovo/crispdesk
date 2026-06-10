// Unit tests for the pure pairing/TURN/rate-limit helpers.
// Run with: npm test   (compiles to dist/ then `node --test`).
import { test } from "node:test";
import assert from "node:assert/strict";
import { createHmac } from "node:crypto";

import {
  CODE_ALPHABET,
  generatePairingCode,
  turnCredentials,
  rateLimitCheck,
} from "../dist/pairing.js";

test("generatePairingCode: length, alphabet, determinism", () => {
  // Injected RNG that always returns 0 -> all first-alphabet char.
  const code0 = generatePairingCode(8, () => 0);
  assert.equal(code0.length, 8);
  assert.equal(code0, CODE_ALPHABET[0].repeat(8));

  // Every char must be from the alphabet, for a real-ish RNG.
  let i = 0;
  const seq = generatePairingCode(8, (max) => i++ % max);
  for (const ch of seq) assert.ok(CODE_ALPHABET.includes(ch), `'${ch}' in alphabet`);

  // No ambiguous characters in the alphabet.
  for (const bad of "01OIL") assert.ok(!CODE_ALPHABET.includes(bad), `no '${bad}'`);
});

test("turnCredentials: coturn use-auth-secret format + valid HMAC", () => {
  const secret = "test-secret";
  const nowSec = 1_700_000_000;
  const ttl = 3600;
  const { username, credential } = turnCredentials(secret, ttl, nowSec);

  assert.equal(username, `${nowSec + ttl}:rcd`);
  // Credential must be base64(HMAC-SHA1(secret, username)) — what coturn verifies.
  const expected = createHmac("sha1", secret).update(username).digest("base64");
  assert.equal(credential, expected);
  // Determinism.
  assert.deepEqual(turnCredentials(secret, ttl, nowSec), { username, credential });
});

test("rateLimitCheck: allows up to max, blocks over, prunes window", () => {
  const windowMs = 60_000;
  const max = 3;

  // First three attempts at t=0 are allowed; the fourth is blocked.
  let attempts = [];
  for (let n = 1; n <= 4; n++) {
    const { allowed, recent } = rateLimitCheck(attempts, 0, windowMs, max);
    attempts = recent;
    assert.equal(allowed, n <= max, `attempt ${n} allowed=${n <= max}`);
  }

  // After the window elapses, old attempts are pruned and we're allowed again.
  const { allowed, recent } = rateLimitCheck(attempts, windowMs + 1, windowMs, max);
  assert.equal(allowed, true);
  assert.equal(recent.length, 1); // only the new attempt remains
});
