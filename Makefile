build:
	cargo build

install:
	install -m u=rwx,g=rx,o=rx target/debug/blockvisord /usr/bin/
	install -m u=rwx,g=rx,o=rx target/debug/bv /usr/bin/
	install -m u=rwx,g=rx,o=rx data/tmux-jailer /usr/bin/
	install -m u=rw,g=r,o=r data/blockvisor.service /etc/systemd/system/
	install -m u=rw,g=r,o=r data/com.BlockJoy.blockvisor.conf /etc/dbus-1/system.d/
