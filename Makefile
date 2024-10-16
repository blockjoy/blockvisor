build:
	cargo build -p blockvisord --target x86_64-unknown-linux-musl
	cargo build -p babel --target x86_64-unknown-linux-musl

build-release:
	cargo build -p blockvisord --target x86_64-unknown-linux-musl --release
	strip target/x86_64-unknown-linux-musl/release/bv
	strip target/x86_64-unknown-linux-musl/release/bib
	strip target/x86_64-unknown-linux-musl/release/bvup
	strip target/x86_64-unknown-linux-musl/release/blockvisord
	strip target/x86_64-unknown-linux-musl/release/blockvisord-dev
	cargo build -p babel --target x86_64-unknown-linux-musl --release
	strip target/x86_64-unknown-linux-musl/release/babel
	strip target/x86_64-unknown-linux-musl/release/babel_job_runner

bundle-base: build-release
	rm -rf /tmp/bundle
	mkdir -p /tmp/bundle/blockvisor/bin /tmp/bundle/blockvisor/services
	cp target/x86_64-unknown-linux-musl/release/bv /tmp/bundle/blockvisor/bin
	cp target/x86_64-unknown-linux-musl/release/blockvisord /tmp/bundle/blockvisor/bin
	cp target/x86_64-unknown-linux-musl/release/installer /tmp/bundle
	cp -r target/x86_64-unknown-linux-musl/release/sh_complete /tmp/bundle/sh_complete
	cp bv/data/blockvisor.service /tmp/bundle/blockvisor/services
	mkdir -p /tmp/bundle/babel/bin
	cp target/x86_64-unknown-linux-musl/release/babel /tmp/bundle/babel/bin
	cp target/x86_64-unknown-linux-musl/release/babel_job_runner /tmp/bundle/babel/bin
	cp target/x86_64-unknown-linux-musl/release/bvup /tmp/bvup

bundle: bundle-base
	rm -rf /tmp/bundle.tar.gz
	tar -C /tmp -czvf /tmp/bundle.tar.gz bundle

bundle-dev: bundle-base
	cp target/x86_64-unknown-linux-musl/release/blockvisord-dev /tmp/bundle/blockvisor/bin/blockvisord
	cp target/x86_64-unknown-linux-musl/release/bib /tmp/bundle/blockvisor/bin
	rm -rf /tmp/bundle-dev.tar.gz
	tar -C /tmp -czvf /tmp/bundle-dev.tar.gz bundle

ci-setup: bundle-base
	cp target/x86_64-unknown-linux-musl/release/bib /tmp/bundle/blockvisor/bin
	systemctl stop blockvisor.service || true
	rm -rf /opt/blockvisor
	/tmp/bundle/installer
	docker build -t test_v1 bv/tests/image_v1
	docker build -t test_v2 bv/tests/image_v2
	bib config test-token
	systemctl start blockvisor.service

ci-cleanup:
	bv node rm --all --yes || true
	bv stop || true
	apptainer instance stop -a || true
	pkill -9 babel || true
	pkill -9 babel_job_runner || true
	umount -A --recursive /var/lib/blockvisor/nodes/*/rootfs || true
	umount -A --recursive /tmp/*/var/lib/blockvisor/nodes/*/rootfs || true
	rm -rf /var/lib/blockvisor/nodes/

new-release:
	cargo release --execute $$(git-conventional-commits version)

promote-prod:
	BV_VERSION=$$(cargo metadata --format-version=1 --no-deps | jq -caM '.packages[] | select(.name == "blockvisord") | .version' | tr -d '"'); \
	aws --endpoint-url https://$${AWS_ACCOUNT_ID}.r2.cloudflarestorage.com/ s3 cp \
                s3://bundle-dev/$${BV_VERSION}/bvd-bundle.tgz \
                s3://bundle-prod/$${BV_VERSION}/bvd-bundle.tgz
