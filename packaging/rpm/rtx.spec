Summary: Polyglot runtime manager
Name: rtx
Version: 1.18.1
Release: 1
URL: https://github.com/jdxcode/rtx/
Group: System
License: MIT
Packager: @jdxcode
BuildRoot: /root/rtx

%description
RTX is a polyglot runtime manager

%install
mkdir -p %{buildroot}/usr/bin/
cp /root/rtx/target/release/rtx %{buildroot}/usr/bin
cp /root/rtx/man/man1/rtx.1 %{buildroot}/usr/share/man/man1/rtx.1

%files
/usr/bin/rtx
