build:
	cargo build -p blockvisord --target x86_64-unknown-linux-musl
	cargo build -p babel --target x86_64-unknown-linux-musl

build-release:
	cargo build -p blockvisord --target x86_64-unknown-linux-musl --release
	strip target/x86_64-unknown-linux-musl/release/bv
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
	rm -rf /tmp/bundle-dev.tar.gz
	tar -C /tmp -czvf /tmp/bundle-dev.tar.gz bundle

install: bundle
	rm -rf /opt/blockvisor
	/tmp/bundle/installer

	mkdir -p /mnt/fc
	for image in $$(find /var/lib/blockvisor/images/ -name "*.img"); do \
		echo $$image; \
		mount $$image /mnt/fc; \
		umount /mnt/fc; \
	done
	cp -f bv/tests/babel.rhai /var/lib/blockvisor/images/testing/validator/0.0.1/; \

reinstall:
	systemctl stop blockvisor.service || true
	make install
	systemctl start blockvisor.service

ci-clean:
	bv node rm --all --yes || true
	bv stop || true
	apptainer instance stop -a || true
	pkill -9 babel || true
	pkill -9 babel_job_runner || true
	umount -A --recursive /var/lib/blockvisor/nodes/*/os || true
	umount -A --recursive /tmp/*/var/lib/blockvisor/nodes/*/os || true
	rm -rf /var/lib/blockvisor/nodes/
	rm -f /var/lib/blockvisor/nodes.json

new-release:
	cargo release --execute $$(git-conventional-commits version)

promote-prod:
	BV_VERSION=$$(cargo metadata --format-version=1 --no-deps | jq -caM '.packages[] | select(.name == "blockvisord") | .version' | tr -d '"'); \
	aws --endpoint-url https://$${AWS_ACCOUNT_ID}.r2.cloudflarestorage.com/ s3 cp \
                s3://bundle-dev/$${BV_VERSION}/bvd-bundle.tgz \
                s3://bundle-prod/$${BV_VERSION}/bvd-bundle.tgz