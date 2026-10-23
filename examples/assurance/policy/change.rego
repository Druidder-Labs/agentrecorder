package witnessed.change

import rego.v1

policy_id := "terraform-change"

policy_version := "0.1.0"

test_result := "https://in-toto.io/attestation/test-result/v0.1"

attesters := {"ci"}

agent_accounts := set()

critical_prefixes := [".github/workflows/", ".github/actions/", ".witnessed/"]

critical_files := {"CODEOWNERS", ".github/CODEOWNERS", "docs/CODEOWNERS"}

paths contains p if {
	some f in input.change.files
	p := f.path
}

paths contains p if {
	some f in input.change.files
	p := f.old_path
}

terraform_change if {
	some p in paths
	endswith(p, ".tf")
}

terraform_change if {
	some p in paths
	endswith(p, ".tfvars")
}

terraform_change if {
	some p in paths
	endswith(p, ".terraform.lock.hcl")
}

terraform_change if {
	some p in paths
	endswith(p, ".tf.json")
}

module_change if {
	some f in input.change.terraform
	some t in f.block_types
	startswith(t, "module.")
}

unscanned_change if {
	some f in input.change.terraform
	f.scanned == false
}

iam_change if {
	some f in input.change.terraform
	some t in f.block_types
	startswith(t, "aws_iam_")
}

iam_change if {
	some f in input.change.terraform
	some t in f.block_types
	startswith(t, "data.aws_iam_")
}

critical_change if {
	some p in paths
	some prefix in critical_prefixes
	startswith(p, prefix)
}

critical_change if {
	some p in paths
	p in critical_files
}

results contains a if {
	some a in input.evidence.attestations
	a.predicate_type == test_result
	a.signer in attesters
}

unauthorized contains a.id if {
	some a in input.evidence.attestations
	a.predicate_type == test_result
	not a.signer in attesters
}

reported(a, name) if name in a.predicate.passedTests

reported(a, name) if name in a.predicate.failedTests

reported(a, name) if name in a.predicate.warnedTests

evidence_for(name) := sort([a.id | some a in results; reported(a, name)])

failed(name) if {
	some a in results
	name in a.predicate.failedTests
}

failed(name) if {
	some a in results
	reported(a, name)
	a.predicate.result == "FAILED"
}

passed(name) if {
	some a in results
	a.predicate.result == "PASSED"
	name in a.predicate.passedTests
}

required_checks contains "terraform-fmt" if terraform_change

required_checks contains "terraform-validate" if terraform_change

required_checks contains "terraform-security-scan" if iam_change

required_checks contains "terraform-security-scan" if module_change

required_checks contains "terraform-security-scan" if unscanned_change

required_checks contains "tests" if not terraform_change

check(name) := {"id": sprintf("check.%s", [name]), "status": "failed", "reason": sprintf("CI reported %s as failed for this commit", [name]), "evidence": evidence_for(name)} if {
	failed(name)
} else := {"id": sprintf("check.%s", [name]), "status": "passed", "reason": sprintf("a test-result attestation signed by an authorized CI key reports %s passed for this commit", [name]), "evidence": evidence_for(name)} if {
	passed(name)
} else := {"id": sprintf("check.%s", [name]), "status": "inconclusive", "reason": sprintf("no test-result attestation signed by an authorized CI key (%s) reports %s for this commit%s", [concat(", ", sort(attesters)), name, unauthorized_note]), "evidence": sort(unauthorized)}

unauthorized_note := sprintf("; %d attestation(s) came from keys not authorized to attest checks", [count(unauthorized)]) if {
	count(unauthorized) > 0
} else := ""

requirements contains check(name) if some name in required_checks

approval_reasons contains "IAM resources or policy documents are in the changed Terraform files" if iam_change

approval_reasons contains "the change edits CI workflows, CODEOWNERS or the policy directory" if critical_change

approval_reasons contains "a changed Terraform file calls a module, whose resources the file scan cannot see" if module_change

approval_reasons contains "a changed Terraform JSON file was not scanned for resource types" if unscanned_change

approvers := {login |
	some login in input.evidence.pull_request.independent_approvals
	not login in agent_accounts
}

approval := {"id": "review.independent-approval", "status": "passed", "reason": sprintf("%s; approved at this commit by %s", [concat("; ", sort(approval_reasons)), concat(", ", sort(approvers))]), "evidence": [input.evidence.pull_request.id]} if {
	count(approvers) > 0
} else := {"id": "review.independent-approval", "status": "inconclusive", "reason": sprintf("%s; no approval at this commit from a human who is neither the author nor a commit author", [concat("; ", sort(approval_reasons))]), "evidence": []}

requirements contains approval if count(approval_reasons) > 0

requirements contains {"id": "review.no-changes-requested", "status": "failed", "reason": sprintf("changes requested by %s", [concat(", ", input.evidence.pull_request.changes_requested)]), "evidence": [input.evidence.pull_request.id]} if {
	count(input.evidence.pull_request.changes_requested) > 0
}

requirements contains {"id": "agent.session-complete", "status": "inconclusive", "reason": sprintf("agent session %s for this commit was captured partially (dropped events or no termination record)", [s.session_id]), "evidence": [s.id]} if {
	some s in input.evidence.sessions
	s.linked_to_commit
	s.partial
}
