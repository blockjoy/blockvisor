build:
	cargo build -p blockvisord --target x86_64-unknown-linux-musl
	cargo build -p babel --target x86_64-unknown-linux-musl

build-release:
	cargo build -p blockvisord --target x86_64-unknown-linux-musl --release
	strip target/x86_64-unknown-linux-musl/release/bv
	strip target/x86_64-unknown-linux-musl/release/bvup
	strip target/x86_64-unknown-linux-musl/release/blockvisord
	cargo build -p babel --target x86_64-unknown-linux-musl --release
	strip target/x86_64-unknown-linux-musl/release/babel
	strip target/x86_64-unknown-linux-musl/release/babelsup

get-firecraker: FC_VERSION=v1.1.3
get-firecraker:
	rm -rf /tmp/fc
	mkdir -p /tmp/fc
	cd /tmp/fc; curl -L https://github.com/firecracker-microvm/firecracker/releases/download/${FC_VERSION}/firecracker-${FC_VERSION}-x86_64.tgz | tar -xz
	cp /tmp/fc/release-${FC_VERSION}-x86_64/firecracker-${FC_VERSION}-x86_64 /tmp/fc/firecracker
	cp /tmp/fc/release-${FC_VERSION}-x86_64/jailer-${FC_VERSION}-x86_64 /tmp/fc/jailer

bundle: get-firecraker build-release
	rm -rf /tmp/bundle.tar.gz
	rm -rf /tmp/bundle
	rm -rf /tmp/bvup
	mkdir -p /tmp/bundle/blockvisor/bin /tmp/bundle/blockvisor/services
	cp target/x86_64-unknown-linux-musl/release/bv /tmp/bundle/blockvisor/bin
	cp target/x86_64-unknown-linux-musl/release/blockvisord /tmp/bundle/blockvisor/bin
	cp target/x86_64-unknown-linux-musl/release/installer /tmp/bundle
	cp bv/data/tmux.service /tmp/bundle/blockvisor/services
	cp bv/data/blockvisor.service /tmp/bundle/blockvisor/services
	mkdir -p /tmp/bundle/babel/bin
	cp target/x86_64-unknown-linux-musl/release/babel /tmp/bundle/babel/bin
	mkdir -p /tmp/bundle/firecracker/bin
	cp /tmp/fc/firecracker /tmp/bundle/firecracker/bin
	cp /tmp/fc/jailer /tmp/bundle/firecracker/bin
	tar -C /tmp -czvf /tmp/bundle.tar.gz bundle
	cp target/x86_64-unknown-linux-musl/release/bvup /tmp/bvup

tag: CARGO_VERSION = $(shell grep '^version' Cargo.toml | sed "s/ //g" | cut -d = -f 2 | sed "s/\"//g")
tag: GIT_VERSION = $(shell git describe --tags)
tag:
	@if [ "${CARGO_VERSION}" = "${GIT_VERSION}" ]; then echo "Version ${CARGO_VERSION} already tagged!"; \
	else git tag -a ${CARGO_VERSION} -m "Set version ${CARGO_VERSION}"; git push origin ${CARGO_VERSION}; \
	fi

install: bundle
	rm -rf /opt/blockvisor
	/tmp/bundle/installer

	systemctl daemon-reload
	systemctl enable blockvisor.service
	for image in $$(find /var/lib/blockvisor/images/ -name "*.img"); do \
		echo $$image; \
		mount $$image /mnt/fc; \
		rm -f /mnt/fc/usr/bin/babel; \
		install -m u=rwx,g=rx,o=rx target/x86_64-unknown-linux-musl/release/babelsup /mnt/fc/usr/bin/; \
		install -m u=rw,g=r,o=r babel/data/babelsup.service /mnt/fc/etc/systemd/system/; \
		rm -f /mnt/fc/etc/systemd/system/multi-user.target.wants/babelsup.service; \
		ln -s /mnt/fc/etc/systemd/system/babelsup.service /mnt/fc/etc/systemd/system/multi-user.target.wants/babelsup.service; \
		umount /mnt/fc; \
	done
	for babel_conf in $$(find /var/lib/blockvisor/images/ -name "*.toml"); do \
		echo $$babel_conf; \
		cp -f babel/protocols/helium/helium-validator.toml $$babel_conf; \
	done

reinstall:
	systemctl stop blockvisor.service
	make install
	systemctl start blockvisor.service

net-clean:
	for i in {1..5000}; do ip link delete bv$i type tuntap; done
