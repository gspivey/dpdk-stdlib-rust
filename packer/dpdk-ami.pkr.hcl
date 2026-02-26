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
  default = "c5n.large"
}

variable "ami_name_prefix" {
  type    = string
  default = "dpdk-stdlib-rust"
}

source "amazon-ebs" "dpdk" {
  region        = var.aws_region
  instance_type = var.instance_type

  source_ami_filter {
    filters = {
      name                = "al2023-ami-*-x86_64"
      root-device-type    = "ebs"
      virtualization-type = "hvm"
    }
    most_recent = true
    owners      = ["amazon"]
  }

  ssh_username = "ec2-user"
  ssh_timeout  = "10m"

  # Ensure Packer instance gets a public IP for SSH access
  associate_public_ip_address = true

  ami_name        = "${var.ami_name_prefix}-dpdk-${var.dpdk_version}-{{timestamp}}"
  ami_description = "Amazon Linux 2023 with DPDK ${var.dpdk_version}, Rust toolchain, and test dependencies pre-installed"

  tags = {
    Name        = "${var.ami_name_prefix}-dpdk-${var.dpdk_version}"
    DpdkVersion = var.dpdk_version
    BaseOS      = "Amazon Linux 2023"
    ManagedBy   = "packer"
    Repository  = "dpdk-stdlib-rust"
  }

  launch_block_device_mappings {
    device_name           = "/dev/xvda"
    volume_size           = 30
    volume_type           = "gp3"
    delete_on_termination = true
  }
}

build {
  sources = ["source.amazon-ebs.dpdk"]

  # Copy the DPDK install script to the instance
  provisioner "file" {
    source      = "../scripts/install_dpdk_amazon_linux.sh"
    destination = "/tmp/install_dpdk.sh"
  }

  # System packages and dev tools
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

  # Install Rust toolchain for root (user-data runs as root)
  provisioner "shell" {
    inline = [
      "sudo bash -c 'curl --proto \"=https\" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y'",
      "sudo bash -c 'echo \"export PATH=/root/.cargo/bin:\\$PATH\" >> /etc/profile'",
      "sudo bash -c 'echo \"export HOME=/root\" >> /etc/profile'",
    ]
  }

  # Install DPDK using the existing install script
  provisioner "shell" {
    inline = [
      "chmod +x /tmp/install_dpdk.sh",
      "sudo bash /tmp/install_dpdk.sh /opt",
    ]
  }

  # Configure kernel modules to load at boot
  provisioner "shell" {
    inline = [
      "echo 'uio' | sudo tee /etc/modules-load.d/dpdk.conf",
      "echo 'vfio-pci' | sudo tee -a /etc/modules-load.d/dpdk.conf",
    ]
  }

  # Configure hugepages to be set up at boot
  provisioner "shell" {
    inline = [
      "echo 'vm.nr_hugepages = 1024' | sudo tee /etc/sysctl.d/90-hugepages.conf",
      "sudo mkdir -p /mnt/huge",
      "echo 'hugetlbfs /mnt/huge hugetlbfs defaults 0 0' | sudo tee -a /etc/fstab",
    ]
  }

  # Verify installations - fail the build if DPDK is not properly installed
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
      "echo \"DPDK headers: $(ls /usr/local/include/rte_eal.h 2>/dev/null && echo 'present' || echo 'MISSING')\"",
      "echo \"DPDK devbind: $(ls /usr/local/bin/dpdk-devbind.py 2>/dev/null && echo 'present' || echo 'not found (ok)')\"",
      "echo \"iperf3: $(iperf3 --version 2>&1 | head -1)\"",
      "echo '=== AMI build complete ==='",
    ]
  }

  # Clean SSM agent state so instances launched from this AMI register fresh.
  # Without this, the agent retains the Packer build instance's registration
  # and may fail to re-register in a new VPC/subnet.
  provisioner "shell" {
    inline = [
      "SSM_SVC=$(systemctl list-unit-files --type=service | grep -i ssm | awk '{print $1}' | head -1)",
      "if [ -n \"$SSM_SVC\" ]; then sudo systemctl stop \"$SSM_SVC\" || true; fi",
      "sudo rm -rf /var/lib/amazon/ssm/ipc/ /var/lib/amazon/ssm/Vault/ /var/lib/amazon/ssm/registration",
    ]
  }

  # Clean up to reduce AMI size
  provisioner "shell" {
    inline = [
      "sudo dnf clean all",
      "sudo rm -rf /tmp/install_dpdk.sh",
    ]
  }
}
