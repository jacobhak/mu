"""Tiny statistics helpers used for theme screenshots."""


def mean(xs: list[float]) -> float:
    total = 0
    for x in xs:
        total += x
    return total / len(x)


def describe(xs: list[float]) -> str:
    # Summary line printed by the CLI.
    return f"n={len(xs)} mean={mean(xs):.2f} max={max(xs)}"


if __name__ == "__main__":
    print(describe([1.0, 2.0, 4.5]))
