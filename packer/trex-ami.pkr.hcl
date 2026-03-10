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

variable "trex_version" {
  type    = string
  default = "v3.08"
}

variable "instance_type" {
  type    = string
  default = "c5n.2xlarge"
}

variable "ami_name_prefix" {
  type    = string
  default = "dpdk-stdlib-rust"
}

source "amazon-ebs" "trex" {
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

  associate_public_ip_address = true

  ami_name        = "${var.ami_name_prefix}-trex-${var.trex_version}-{{timestamp}}"
  ami_description = "Amazon Linux 2023 with TRex ${var.trex_version} traffic generator pre-installed"

  tags = {
    Name        = "${var.ami_name_prefix}-trex-${var.trex_version}"
    TrexVersion = var.trex_version
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
  sources = ["source.amazon-ebs.trex"]

  # System packages and dev tools
  provisioner "shell" {
    inline = [
      "echo '=== Installing system packages ==='",
      "sudo dnf update -y",
      "sudo dnf groupinstall -y 'Development Tools'",
      "sudo dnf install -y pciutils numactl numactl-devel python3 python3-pip amazon-ssm-agent",
      "sudo dnf install -y aws-cfn-bootstrap || echo 'Warning: aws-cfn-bootstrap not available, skipping'",
      "sudo systemctl enable amazon-ssm-agent",
    ]
  }

  # Download and install TRex
  provisioner "shell" {
    inline = [
      "echo '=== Installing TRex ${var.trex_version} ==='",
      "cd /opt",
      "TREX_VERSION='${var.trex_version}'",
      "sudo curl -fL --retry 3 --retry-delay 10 \"https://trex-tgn.cisco.com/trex/release/$${TREX_VERSION}.tar.gz\" -o trex.tar.gz",
      "sudo tar -xzf trex.tar.gz",
      "sudo mv $${TREX_VERSION} trex",
      "sudo rm -f trex.tar.gz",
      "echo 'TRex installed to /opt/trex'",
      "ls -la /opt/trex/",
    ]
  }

  # Install TRex Python dependencies
  provisioner "shell" {
    inline = [
      "echo '=== Installing TRex Python dependencies ==='",
      "cd /opt/trex",
      "sudo pip3 install PyYAML scapy || sudo pip3 install --break-system-packages PyYAML scapy",
    ]
  }

  # Configure kernel modules for DPDK (TRex uses DPDK internally)
  provisioner "shell" {
    inline = [
      "echo 'vfio-pci' | sudo tee /etc/modules-load.d/trex.conf",
    ]
  }

  # Configure hugepages
  provisioner "shell" {
    inline = [
      "echo 'vm.nr_hugepages = 1024' | sudo tee /etc/sysctl.d/90-hugepages.conf",
      "sudo mkdir -p /mnt/huge",
      "echo 'hugetlbfs /mnt/huge hugetlbfs defaults 0 0' | sudo tee -a /etc/fstab",
    ]
  }

  # Verify installation
  provisioner "shell" {
    inline = [
      "echo '=== Verification ==='",
      "echo \"TRex binary: $(ls /opt/trex/t-rex-64 2>/dev/null && echo 'present' || echo 'MISSING')\"",
      "echo \"TRex STL lib: $(ls /opt/trex/automation/trex_control_plane/interactive/trex/stl/ 2>/dev/null && echo 'present' || echo 'MISSING')\"",
      "echo \"Python3: $(python3 --version)\"",
      "if [ ! -f /opt/trex/t-rex-64 ]; then echo 'FATAL: TRex binary not found'; exit 1; fi",
      "echo '=== TRex AMI build complete ==='",
    ]
  }

  # Clean SSM agent state for fresh registration
  provisioner "shell" {
    inline = [
      "SSM_SVC=$(systemctl list-unit-files --type=service | grep -i ssm | awk '{print $1}' | head -1)",
      "if [ -n \"$SSM_SVC\" ]; then sudo systemctl stop \"$SSM_SVC\" || true; fi",
      "sudo rm -rf /var/lib/amazon/ssm/ipc/ /var/lib/amazon/ssm/Vault/ /var/lib/amazon/ssm/registration",
    ]
  }

  # Clean up
  provisioner "shell" {
    inline = [
      "sudo dnf clean all",
    ]
  }
}
