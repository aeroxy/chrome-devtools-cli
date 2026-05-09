.PHONY: build check run test clean bump-patch bump-minor bump-major update-formula release

## Build the project (debug)
build:
	cargo build

## Release build — use this before manual testing
release:
	cargo build --release
	zip -j target/release/chrome-devtools-macos-arm64.zip target/release/chrome-devtools

## Type-check without producing a binary
check:
	cargo check

## Run the CLI; pass subcommands via ARGS
##   make run ARGS="list-pages"
##   make run ARGS="screenshot --output out.png"
run:
	cargo run -- $(ARGS)

## Run tests
test:
	cargo test

## Remove build artifacts
clean:
	cargo clean

## Bump the patch version (0.1.3 → 0.1.4) and update all version references
bump-patch:
	@old=$$(grep '^version' Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/'); \
	major=$$(echo $$old | cut -d. -f1); \
	minor=$$(echo $$old | cut -d. -f2); \
	patch=$$(echo $$old | cut -d. -f3); \
	new="$$major.$$minor.$$((patch+1))"; \
	sed -i '' "s/^version = \"$$old\"/version = \"$$new\"/" Cargo.toml; \
	sed -i '' "s/version \"$$old\"/version \"$$new\"/" Formula/chrome-devtools.rb; \
	sed -i '' "s|/$$old/|/$$new/|g" Formula/chrome-devtools.rb; \
	echo "$$old → $$new"

## Bump the minor version (0.1.4 → 0.2.0) and update all version references
bump-minor:
	@old=$$(grep '^version' Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/'); \
	major=$$(echo $$old | cut -d. -f1); \
	minor=$$(echo $$old | cut -d. -f2); \
	new="$$major.$$((minor+1)).0"; \
	sed -i '' "s/^version = \"$$old\"/version = \"$$new\"/" Cargo.toml; \
	sed -i '' "s/version \"$$old\"/version \"$$new\"/" Formula/chrome-devtools.rb; \
	sed -i '' "s|/$$old/|/$$new/|g" Formula/chrome-devtools.rb; \
	echo "$$old → $$new"

## Bump the major version (0.1.4 → 1.0.0) and update all version references
bump-major:
	@old=$$(grep '^version' Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/'); \
	major=$$(echo $$old | cut -d. -f1); \
	new="$$((major+1)).0.0"; \
	sed -i '' "s/^version = \"$$old\"/version = \"$$new\"/" Cargo.toml; \
	sed -i '' "s/version \"$$old\"/version \"$$new\"/" Formula/chrome-devtools.rb; \
	sed -i '' "s|/$$old/|/$$new/|g" Formula/chrome-devtools.rb; \
	echo "$$old → $$new"

## Update Formula/chrome-devtools.rb SHA256 from local release zip (run after release-macos, before upload)
##   make update-formula
update-formula:
	@mac_zip="target/release/chrome-devtools-macos-arm64.zip"; \
	echo "Computing macOS SHA256 …"; \
	mac_sha=$$(shasum -a 256 "$$mac_zip" | cut -d' ' -f1); \
	echo "macOS SHA256: $$mac_sha"; \
	sed -i '' "s/sha256 \"[a-f0-9]*\"/sha256 \"$$mac_sha\"/" Formula/chrome-devtools.rb; \
	echo "Formula/chrome-devtools.rb updated"
