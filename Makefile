build-client:
	cp config.env bin/ethproofs-client/.env
	cargo build --release --manifest-path bin/ethproofs-client/Cargo.toml

build-input-gen:
	cp config.env bin/input-gen-server/.env
	cargo build --release --manifest-path bin/input-gen-server/Cargo.toml

build: build-client build-input-gen
