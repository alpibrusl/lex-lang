.PHONY: hooks mutation-audit

hooks:
	mkdir -p .git/hooks
	cp .githooks/pre-commit .git/hooks/pre-commit
	chmod +x .git/hooks/pre-commit
	@echo "pre-commit hook installed — run 'make hooks' in each clone"

# Check that a lex package's tests can actually fail. Point it at packages,
# not at this repo — lex-lang's own tests are Rust.
#   make mutation-audit PKGS="../lex-trail ../lex-web"
mutation-audit:
	@test -n "$(PKGS)" || { echo "usage: make mutation-audit PKGS=\"../lex-trail ...\""; exit 1; }
	./scripts/mutation-audit.sh $(PKGS)
