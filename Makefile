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
	strip target/x86_64-unknown-linux-musl/release/babelsup

get-firecraker: FC_VERSION=v1.5.0
get-firecraker:
	rm -rf /tmp/fc
	mkdir -p /tmp/fc
	cd /tmp/fc; curl -L https://github.com/firecracker-microvm/firecracker/releases/download/${FC_VERSION}/firecracker-${FC_VERSION}-x86_64.tgz | tar -xz
	cp /tmp/fc/release-${FC_VERSION}-x86_64/firecracker-${FC_VERSION}-x86_64 /tmp/fc/firecracker
	cp /tmp/fc/release-${FC_VERSION}-x86_64/jailer-${FC_VERSION}-x86_64 /tmp/fc/jailer

bundle-base: get-firecraker build-release
	rm -rf /tmp/bundle
	mkdir -p /tmp/bundle/blockvisor/bin /tmp/bundle/blockvisor/services
	cp target/x86_64-unknown-linux-musl/release/bv /tmp/bundle/blockvisor/bin
	cp target/x86_64-unknown-linux-musl/release/blockvisord /tmp/bundle/blockvisor/bin
	cp target/x86_64-unknown-linux-musl/release/installer /tmp/bundle
	cp bv/data/tmux.service /tmp/bundle/blockvisor/services
	cp bv/data/blockvisor.service /tmp/bundle/blockvisor/services
	mkdir -p /tmp/bundle/babel/bin
	cp target/x86_64-unknown-linux-musl/release/babel /tmp/bundle/babel/bin
	cp target/x86_64-unknown-linux-musl/release/babel_job_runner /tmp/bundle/babel/bin
	mkdir -p /tmp/bundle/firecracker/bin
	cp /tmp/fc/firecracker /tmp/bundle/firecracker/bin
	cp /tmp/fc/jailer /tmp/bundle/firecracker/bin
	rm -rf /tmp/babelsup.tar.gz
	rm -rf /tmp/babelsup
	mkdir -p /tmp/babelsup/usr/bin/ /tmp/babelsup/etc/systemd/system/ /tmp/babelsup/etc/systemd/system/multi-user.target.wants/
	cp target/x86_64-unknown-linux-musl/release/babelsup /tmp/babelsup/usr/bin/
	cp babel/data/babelsup.service /tmp/babelsup/etc/systemd/system/
	ln -sr /tmp/babelsup/etc/systemd/system/babelsup.service /tmp/babelsup/etc/systemd/system/multi-user.target.wants/babelsup.service
	tar -C /tmp/babelsup -czvf /tmp/bundle/babelsup.tar.gz .
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
		install -m u=rwx,g=rx,o=rx target/x86_64-unknown-linux-musl/release/babelsup /mnt/fc/usr/bin/; \
		install -m u=rw,g=r,o=r babel/data/babelsup.service /mnt/fc/etc/systemd/system/; \
		ln -srf /mnt/fc/etc/systemd/system/babelsup.service /mnt/fc/etc/systemd/system/multi-user.target.wants/babelsup.service; \
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
	pkill -9 babel || true
	pkill -9 babel_job_runner || true
	umount -A --recursive /var/lib/blockvisor/bare/*/os || true
	rm -rf /var/lib/blockvisor/bare/
	pkill -9 firecracker || true
	rm -rf /var/lib/blockvisor/firecracker/
	rm -rf /var/lib/blockvisor/nodes/
	rm -f /var/lib/blockvisor/nodes.json
	for i in $$(seq 1 100); do ip link delete bv$$i type tuntap; done || true
