.PHONY: help setup build test lint audit verify plan

help:
	@echo "BoringBuilder"
	@echo ""
	@echo "Targets: setup, build, test, lint, audit, verify, plan"

setup:
	bin/setup

build:
	bundle exec rake build

test:
	bundle exec rake test

lint:
	bundle exec rubocop

audit:
	bundle exec bundle-audit check --update

verify:
	bin/ci

plan:
	bundle exec boringbuilder build --dry-run "$${PROJECT:-.}"
