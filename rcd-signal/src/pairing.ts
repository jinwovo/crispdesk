// Pure, side-effect-free helpers for pairing codes, TURN credentials, and rate
// limiting. Extracted from server.ts so they can be unit-tested in isolation
// (the RNG and clock are injected, so tests are deterministic).

import { createHmac } from "node:crypto";

/** Unambiguous alphabet (no 0/O/1/I/L) for human-typed codes. */
export const CODE_ALPHABET = "23456789ABCDEFGHJKMNPQRSTUVWXYZ";

/** Generate a pairing code of `length` chars using the injected RNG (randomInt-like:
 *  `randInt(max)` returns an int in [0, max)). */
export function generatePairingCode(length: number, randInt: (max: number) => number): string {
  let code = "";
  for (let i = 0; i < length; i++) {
    code += CODE_ALPHABET[randInt(CODE_ALPHABET.length)];
  }
  return code;
}

/** coturn `use-auth-secret` ephemeral credentials. `nowSec` is unix seconds; the
 *  username embeds the expiry so coturn can validate it without server state. */
export function turnCredentials(
  secret: string,
  ttlSec: number,
  nowSec: number,
): { username: string; credential: string } {
  const username = `${nowSec + ttlSec}:rcd`;
  const credential = createHmac("sha1", secret).update(username).digest("base64");
  return { username, credential };
}

/** Sliding-window rate-limit decision. Returns the pruned timestamp list (with `nowMs`
 *  appended) and whether this attempt is allowed (<= `max` within `windowMs`). */
export function rateLimitCheck(
  attempts: readonly number[],
  nowMs: number,
  windowMs: number,
  max: number,
): { allowed: boolean; recent: number[] } {
  const recent = attempts.filter((t) => nowMs - t < windowMs);
  recent.push(nowMs);
  return { allowed: recent.length <= max, recent };
}

/**
 * Origin allowlist decision for WS upgrades. `allowList` is the parsed set of
 * permitted Origin header values (already trimmed; empty entries dropped).
 *
 * Policy:
 *   - An EMPTY allowList means "no restriction" — allow everything (the default
 *     when ALLOWED_ORIGINS is unset). This preserves existing behaviour.
 *   - A non-empty allowList permits only an EXACT-match Origin. A missing/empty
 *     Origin header (e.g. a non-browser client like the Rust host, which sends no
 *     Origin) is allowed: the allowlist targets browser cross-origin abuse, not
 *     native peers, and rejecting it would break the host.
 *
 * Pure and side-effect-free so it can be unit-tested in isolation.
 */
export function isOriginAllowed(
  origin: string | undefined | null,
  allowList: readonly string[],
): boolean {
  // No restriction configured -> allow all (unchanged default behaviour).
  if (allowList.length === 0) return true;
  // Non-browser clients (host) send no Origin header; do not block them.
  if (origin === undefined || origin === null || origin === "") return true;
  return allowList.includes(origin);
}

/** Parse a comma-separated ALLOWED_ORIGINS value into a trimmed, non-empty list. */
export function parseAllowedOrigins(raw: string | undefined): string[] {
  return (raw ?? "")
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);
}
