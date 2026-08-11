import sys

value = sys.stdin.read().strip()
raise SystemExit(0 if value.isdigit() else 1)
