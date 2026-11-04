#!/bin/sh
# Stands in for `omp token` in the MCP auth tests: `nosuch` fails, anything else prints its argv and broker env.
case " $* " in *nosuch*) exit 1 ;; esac
printf '%s|%s|%s\n' "$*" "$OMP_AUTH_BROKER_URL" "$OMP_AUTH_BROKER_TOKEN"
