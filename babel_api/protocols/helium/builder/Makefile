# version=1.15.6 tag=devnet_validator1.15.6 make compile unmount zip || make unmount clean

all:

build_image:
	fallocate -l 5G helium-${version}.img

add_filesystem:
	mkfs.ext4 helium-${version}.img

mount:
	mkdir -p /mnt/helium
	mount helium-${version}.img /mnt/helium

unmount:
	-umount /mnt/helium

bootstrap:
	debootstrap bullseye /mnt/helium
	echo 'root:root' | chroot /mnt/helium chpasswd
	chroot /mnt/helium bash -c "apt-get update -y"
	chroot /mnt/helium bash -c "apt install -y libsodium-dev libdbus-1-dev"
	mkdir -p /mnt/helium/blockjoy/miner

prep_system:
	apt-get update
	-apt-get -y install build-essential automake bison git curl bzip2 wget cmake libsodium-dev autoconf gf-complete-tools libtool-bin libssl-dev dbus libdbus-1-dev
	wget https://packages.erlang-solutions.com/erlang/debian/pool/esl-erlang_24.3.3-1~ubuntu~focal_amd64.deb
	-dpkg -i ./esl-erlang_24.3.3-1~ubuntu~focal_amd64.deb
	apt -y --fix-broken install
	curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y

compile: build_image add_filesystem mount bootstrap prep_system
	PATH=/root/.cargo/bin:${PATH}
	if [ ! -d "miner" ]; then \
		git clone https://github.com/helium/miner; \
	fi

	cd miner; git checkout ${tag}; ./rebar3 as validator tar -v ${tag} -n miner; tar -zxvf _build/validator/rel/*/*.tar.gz -C /mnt/helium/blockjoy/miner

clean:
	rm -f helium*.img
	rm -f esl-erlang_24.3.3-1~ubuntu~focal_amd64.deb*

zip:
	tar cvvzf helium-${version}.gz helium-${version}.img
