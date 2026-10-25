.DEFAULT_GOAL := build

.PHONY: update build test install clean rust-doc uml flows interactive completions

CARGO_TARGET_DIR ?= target
BASH_COMPLETION := $(CARGO_TARGET_DIR)/asb.bash
ZSH_COMPLETION := $(CARGO_TARGET_DIR)/_asb
ASB_BIN := $(CARGO_TARGET_DIR)/debug/asb
AUTO_UML_VERSION := 0.7.2
AUTO_UML_ROOT := target/tools/auto-uml
AUTO_UML := $(AUTO_UML_ROOT)/bin/auto-uml
UML_RAW_DOT := target/asb-type-diagram.raw.dot
UML_DOT := docs/asb-type-diagram.dot
UML_SVG := docs/asb-type-diagram.svg
UML_RAW_MERMAID := target/asb-type-diagram.raw.mmd
UML_MERMAID := docs/asb-type-diagram.mmd
FLOW_SCRIPT := scripts/generate_function_flows.py
DATAFLOW_DOT := docs/asb-dataflow.dot
DATAFLOW_SVG := docs/asb-dataflow.svg
CONTROLFLOW_DOT := docs/asb-control-flow.dot
CONTROLFLOW_SVG := docs/asb-control-flow.svg
INTERACTIVE_TEMPLATE := scripts/interactive_graph_template.html
INTERACTIVE_CSS := scripts/interactive_graph.css
INTERACTIVE_JS := scripts/interactive_graph.js
TYPE_HTML := docs/asb-type-diagram.html
DATAFLOW_HTML := docs/asb-dataflow.html
CONTROLFLOW_HTML := docs/asb-control-flow.html

update:
	bash scripts/update_checkout.sh

build:
	cargo build --locked
	$(MAKE) rust-doc
	$(MAKE) completions

# The generated shells are deliberately produced from the same Clap command
# tree as `asb --help`, after every build.  Keep this target separate so CI
# and package builders can verify the install artifacts without regenerating
# unrelated documentation.
completions:
	cargo build --locked -p asb-cli
	$(ASB_BIN) --__generate-completion bash > "$(BASH_COMPLETION)"
	$(ASB_BIN) --__generate-completion zsh > "$(ZSH_COMPLETION)"

rust-doc:
	cargo doc --locked --workspace --no-deps --document-private-items

$(AUTO_UML):
	mkdir -p "$(AUTO_UML_ROOT)"
	cargo install auto-uml --version "$(AUTO_UML_VERSION)" --locked --root "$(AUTO_UML_ROOT)"

uml: $(AUTO_UML)
	command -v dot >/dev/null || (echo "Graphviz 'dot' is required for UML generation (install the graphviz package)." >&2; exit 1)
	$(AUTO_UML) --lang rust --source-code crates --format graphviz --destination "$(UML_RAW_DOT)"
	python3 scripts/organize_uml_dot.py "$(UML_RAW_DOT)" "$(UML_DOT)"
	dot -Tsvg -o "$(UML_SVG)" "$(UML_DOT)"
	$(AUTO_UML) --lang rust --source-code crates --format mermaid --destination "$(UML_RAW_MERMAID)"
	python3 scripts/normalize_uml.py "$(UML_RAW_MERMAID)" "$(UML_MERMAID)"

flows:
	command -v dot >/dev/null || (echo "Graphviz 'dot' is required for flow-diagram generation (install the graphviz package)." >&2; exit 1)
	python3 "$(FLOW_SCRIPT)" crates "$(DATAFLOW_DOT)" "$(CONTROLFLOW_DOT)"
	dot -Tsvg -o "$(DATAFLOW_SVG)" "$(DATAFLOW_DOT)"
	dot -Tsvg -o "$(CONTROLFLOW_SVG)" "$(CONTROLFLOW_DOT)"

interactive: flows
	python3 scripts/render_interactive_graph.py "$(INTERACTIVE_TEMPLATE)" "$(INTERACTIVE_CSS)" "$(INTERACTIVE_JS)" "$(UML_SVG)" "$(TYPE_HTML)" "ASB Rust type diagram"
	python3 scripts/render_interactive_graph.py "$(INTERACTIVE_TEMPLATE)" "$(INTERACTIVE_CSS)" "$(INTERACTIVE_JS)" "$(DATAFLOW_SVG)" "$(DATAFLOW_HTML)" "ASB Rust dataflow diagram"
	python3 scripts/render_interactive_graph.py "$(INTERACTIVE_TEMPLATE)" "$(INTERACTIVE_CSS)" "$(INTERACTIVE_JS)" "$(CONTROLFLOW_SVG)" "$(CONTROLFLOW_HTML)" "ASB Rust control-flow diagram"

test:
	cargo test --locked
	python3 scripts/test_update_checkout.py

install: build
	mkdir -p "$$HOME/.local/bin"
	mkdir -p "$$HOME/.local/share/bash-completion/completions"
	mkdir -p "$$HOME/.local/share/zsh/site-functions"
	install -m 755 target/debug/asb "$$HOME/.local/bin/asb"
	install -m 644 "$(BASH_COMPLETION)" "$$HOME/.local/share/bash-completion/completions/asb"
	install -m 644 "$(ZSH_COMPLETION)" "$$HOME/.local/share/zsh/site-functions/_asb"

clean:
	cargo clean
