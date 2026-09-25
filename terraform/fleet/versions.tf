terraform {
  required_version = ">= 1.4"

  required_providers {
    scaleway = {
      source = "scaleway/scaleway"
      # Partner Premier tier. Pinned to a minor so a provider release cannot change
      # what `for_each` does to a live fleet between one apply and the next.
      version = "~> 2.83"
    }
  }
}

provider "scaleway" {
  # Credentials come from SCW_ACCESS_KEY / SCW_SECRET_KEY / SCW_DEFAULT_PROJECT_ID
  # in the environment — never from this file, and never from a tfvars file that
  # could end up in a diff.
  zone   = var.zone
  region = var.region
}
