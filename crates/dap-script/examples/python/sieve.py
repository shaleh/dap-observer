"""First N primes via the Sieve of Eratosthenes.

A deliberately step-through-friendly implementation. It keeps a lot of
mutating state on purpose so there is plenty to watch in a debugger:

  * ``is_prime``   - a flat list of bools that flips False over time
  * ``crossed_off`` - a dict[int, list[int]] (nested! good for testing
                      variable-tree expansion in a DAP client)
  * ``p`` / ``multiple`` / ``eliminated`` - loop-local scalars and lists
"""

from math import log


def upper_bound_for_nth_prime(n: int) -> int:
    """An upper bound for the n-th prime, so we know how far to sieve.

    Uses Rosser's theorem (valid for n >= 6); small n is hard-coded.
    """
    if n < 6:
        return 15  # comfortably covers the first 5 primes (2, 3, 5, 7, 11)
    return int(n * (log(n) + log(log(n)))) + 1


def sieve_up_to(limit: int) -> list[int]:
    """Return every prime <= *limit* using the Sieve of Eratosthenes."""
    is_prime = [True] * (limit + 1)
    is_prime[0] = is_prime[1] = False

    # Record which composites each prime eliminated. Nothing needs this
    # for the algorithm -- it exists to give the debugger a nested
    # structure to expand and watch.
    crossed_off: dict[int, list[int]] = {}

    for p in range(2, limit + 1):
        if not is_prime[p]:
            continue

        eliminated: list[int] = []
        for multiple in range(p * p, limit + 1, p):
            if is_prime[multiple]:
                is_prime[multiple] = False
                eliminated.append(multiple)  # <-- a good inner breakpoint

        if eliminated:
            crossed_off[p] = eliminated

    primes = [i for i, prime in enumerate(is_prime) if prime]
    return primes


def first_n_primes(n: int) -> list[int]:
    """Return the first *n* primes as a list."""
    limit = upper_bound_for_nth_prime(n)
    primes = sieve_up_to(limit)  # <-- a good outer breakpoint
    return primes[:n]


if __name__ == "__main__":
    count = 20
    result = first_n_primes(count)
    print(f"First {count} primes: {result}")
