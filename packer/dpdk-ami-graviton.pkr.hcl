packer {
  required_plugins {
    amazon = {
      version = ">= 1.2.0"
      source  = "github.com/hashicorp/amazon"
    }
  }
}

variable "aws_region" {
  type    = string
  default = "us-east-1"
}

variable "dpdk_version" {
  type    = string
  default = "22.11.6"
}

variable "instance_type" {
  type    = string
  default = "c7g.large"
}

variable "ami_name_prefix" {
  type    = string
  default = "dpdk-stdlib-rust-graviton"
}

source "amazon-ebs" "dpdk-graviton" {
  region        = var.aws_region
  instance_type = var.instance_type

  source_ami_filter {
    filters = {
      name                = "al2023-ami-*-arm64"
      root-device-type    = "ebs"
      virtualization-type = "hvm"
    }
    most_recent = true
    owners      = ["amazon"]
  }

  ssh_username = "ec2-user"
  ssh_timeout  = "10m"

  associate_public_ip_address = true

  ami_name        = "${var.ami_name_prefix}-dpdk-${var.dpdk_version}-{{timestamp}}"
  ami_description = "Amazon Linux 2023 arm64 with DPDK ${var.dpdk_version}, Rust toolchain, and test dependencies pre-installed (Graviton)"

  tags = {
    Name        = "${var.ami_name_prefix}-dpdk-${var.dpdk_version}"
    DpdkVersion = var.dpdk_version
    BaseOS      = "Amazon Linux 2023 arm64"
    ManagedBy   = "packer"
    Repository  = "dpdk-stdlib-rust"
    Arch        = "arm64"
  }

  launch_block_device_mappings {
    device_name           = "/dev/xvda"
    volume_size           = 30
    volume_type           = "gp3"
    delete_on_termination = true
  }
}

build {
  sources = ["source.amazon-ebs.dpdk-graviton"]

  provisioner "file" {
    source      = "../scripts/install_dpdk_amazon_linux.sh"
    destination = "/tmp/install_dpdk.sh"
  }

  provisioner "shell" {
    inline = [
      "echo '=== Installing system packages ==='",
      "sudo dnf update -y",
      "sudo dnf groupinstall -y 'Development Tools'",
      "sudo dnf install -y git pciutils iperf3 clang-devel amazon-ssm-agent",
      "sudo dnf install -y aws-cfn-bootstrap || echo 'Warning: aws-cfn-bootstrap not available, skipping'",
      "sudo systemctl enable amazon-ssm-agent",
    ]
  }

  provisioner "shell" {
    inline = [
      "sudo bash -c 'curl --proto \"=https\" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y'",
      "sudo bash -c 'echo \"export PATH=/root/.cargo/bin:\\$PATH\" >> /etc/profile'",
      "sudo bash -c 'echo \"export HOME=/root\" >> /etc/profile'",
    ]
  }

  provisioner "shell" {
    inline = [
      "chmod +x /tmp/install_dpdk.sh",
      "sudo bash /tmp/install_dpdk.sh /opt",
    ]
  }

  provisioner "shell" {
    inline = [
      "echo 'uio' | sudo tee /etc/modules-load.d/dpdk.conf",
      "echo 'vfio-pci' | sudo tee -a /etc/modules-load.d/dpdk.conf",
    ]
  }

  provisioner "shell" {
    inline = [
      "echo 'vm.nr_hugepages = 1024' | sudo tee /etc/sysctl.d/90-hugepages.conf",
      "sudo mkdir -p /mnt/huge",
      "echo 'hugetlbfs /mnt/huge hugetlbfs defaults 0 0' | sudo tee -a /etc/fstab",
    ]
  }

  provisioner "shell" {
    inline = [
      "echo '=== Verification ==='",
      "echo \"Rust: $(sudo /root/.cargo/bin/rustc --version)\"",
      "",
      "DPDK_LIB_COUNT=$(ls /usr/local/lib/librte_* 2>/dev/null | wc -l)",
      "echo \"DPDK libs: $DPDK_LIB_COUNT libraries\"",
      "if [ \"$DPDK_LIB_COUNT\" -lt 10 ]; then echo 'FATAL: DPDK libraries not installed correctly'; exit 1; fi",
      "",
      "DPDK_VERSION=$(PKG_CONFIG_PATH=/usr/local/lib/pkgconfig pkg-config --modversion libdpdk 2>/dev/null || echo '')",
      "echo \"DPDK version: $DPDK_VERSION\"",
      "if [ -z \"$DPDK_VERSION\" ]; then echo 'FATAL: pkg-config cannot find libdpdk'; exit 1; fi",
      "",
      "echo \"Arch: $(uname -m)\"",
      "echo \"DPDK headers: $(ls /usr/local/include/rte_eal.h 2>/dev/null && echo 'present' || echo 'MISSING')\"",
      "echo \"iperf3: $(iperf3 --version 2>&1 | head -1)\"",
      "echo '=== AMI build complete ==='",
    ]
  }

  provisioner "shell" {
    inline = [
      "SSM_SVC=$(systemctl list-unit-files --type=service | grep -i ssm | awk '{print $1}' | head -1)",
      "if [ -n \"$SSM_SVC\" ]; then sudo systemctl stop \"$SSM_SVC\" || true; fi",
      "sudo rm -rf /var/lib/amazon/ssm/ipc/ /var/lib/amazon/ssm/Vault/ /var/lib/amazon/ssm/registration",
    ]
  }

  provisioner "shell" {
    inline = [
      "sudo dnf clean all",
      "sudo rm -rf /tmp/install_dpdk.sh",
    ]
  }
}
