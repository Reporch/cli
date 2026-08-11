import sys

tokens = sys.stdin.read().split()
raise SystemExit(42 if len(tokens) == 1 and tokens[0].lstrip("-").isdigit() else 43)
