#!/usr/bin/env python3
"""Verify crates.io serves the expected version for one or more crates.

crates.io's index propagation is eventually-consistent: a `cargo publish`
that succeeded can take a minute or two before `GET /api/v1/crates/<name>`
returns the new `max_version`. This script polls until it does, or fails
after a bounded number of attempts.
"""

from __future__ import annotations

import argparse
import json
import sys
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from typing import Callable


USER_AGENT = "yolop-publish-verifier/1.0"


class RegistryResponseError(RuntimeError):
    """crates.io answered, but not with the shape we need yet."""


@dataclass
class HttpResponse:
    status: int
    body: bytes


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Verify crates.io publishes are visible at the expected version."
    )
    parser.add_argument("--expected", required=True, help="Expected published version")
    parser.add_argument(
        "--attempts",
        type=int,
        default=6,
        help="Number of fetch attempts per crate after the initial workflow delay",
    )
    parser.add_argument(
        "--delay-seconds",
        type=float,
        default=10.0,
        help="Delay between retry attempts",
    )
    parser.add_argument(
        "--timeout-seconds",
        type=float,
        default=30.0,
        help="HTTP timeout per request",
    )
    parser.add_argument("crates", nargs="+", help="Crate names to verify")
    return parser.parse_args()


def fetch_response(crate: str, timeout_seconds: float) -> HttpResponse:
    request = urllib.request.Request(
        f"https://crates.io/api/v1/crates/{crate}",
        headers={"User-Agent": USER_AGENT},
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout_seconds) as response:
            return HttpResponse(status=response.status, body=response.read())
    except urllib.error.HTTPError as error:
        # HTTP-level errors (4xx/5xx) still have a body — let the parser
        # decide whether to retry based on the registry's error payload.
        return HttpResponse(status=error.code, body=error.read())
    except (urllib.error.URLError, TimeoutError, ConnectionError) as error:
        # DNS, connection reset, TLS, read timeout — transient. Re-raise as
        # the retryable error so fetch_max_version_with_retries waits and
        # tries again instead of failing the publish on a blip.
        raise RegistryResponseError(
            f"{crate}: crates.io request failed before a response was received: {error}"
        ) from error


def extract_max_version(crate: str, response: HttpResponse) -> str:
    try:
        payload = json.loads(response.body)
    except json.JSONDecodeError as error:
        raise RegistryResponseError(
            f"{crate}: crates.io returned invalid JSON (status {response.status}): {error}"
        ) from error

    crate_data = payload.get("crate")
    if response.status != 200:
        detail = summarize_error_payload(payload)
        raise RegistryResponseError(
            f"{crate}: crates.io returned HTTP {response.status}: {detail}"
        )
    if not isinstance(crate_data, dict):
        detail = summarize_error_payload(payload)
        raise RegistryResponseError(
            f"{crate}: crates.io payload missing 'crate' object: {detail}"
        )
    max_version = crate_data.get("max_version")
    if not isinstance(max_version, str) or not max_version:
        raise RegistryResponseError(
            f"{crate}: crates.io payload missing 'crate.max_version'"
        )
    return max_version


def summarize_error_payload(payload: object) -> str:
    if isinstance(payload, dict):
        errors = payload.get("errors")
        if isinstance(errors, list) and errors:
            details = [
                error.get("detail", str(error))
                for error in errors
                if isinstance(error, dict)
            ]
            if details:
                return "; ".join(details)
        return json.dumps(payload, sort_keys=True)[:300]
    return repr(payload)[:300]


def fetch_max_version_with_retries(
    crate: str,
    attempts: int,
    delay_seconds: float,
    timeout_seconds: float,
    fetcher: Callable[[str, float], HttpResponse] = fetch_response,
) -> str:
    last_error: RegistryResponseError | None = None
    for attempt in range(1, attempts + 1):
        try:
            return extract_max_version(crate, fetcher(crate, timeout_seconds))
        except RegistryResponseError as error:
            last_error = error
            if attempt == attempts:
                break
            print(
                f"Retry {attempt}/{attempts - 1} for {crate}: {error}",
                file=sys.stderr,
            )
            time.sleep(delay_seconds)

    assert last_error is not None
    raise last_error


def main() -> int:
    args = parse_args()
    all_ok = True

    for crate in args.crates:
        actual = fetch_max_version_with_retries(
            crate=crate,
            attempts=args.attempts,
            delay_seconds=args.delay_seconds,
            timeout_seconds=args.timeout_seconds,
        )
        if actual == args.expected:
            print(f"OK  {crate}@{actual} on crates.io")
        else:
            print(
                f"FAIL {crate}: expected {args.expected}, got {actual}",
                file=sys.stderr,
            )
            all_ok = False

    if not all_ok:
        print(
            "::error::Some crates.io packages were not published correctly",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
