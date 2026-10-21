import {
  workflow, configuration, event, repo, packages, checks, jobs, reviews, releases,
  OperationRef, OperationRequest, RegistryPublishRequest, WriteResult,
} from "runnerless/v1";

const program = workflow().releasePlease();

export function configure(): void {
  configuration.titlePolicy();
  program.configure();
  configuration.workflow("crate-publishing", ["workflow_run", "runnerless_completion"]);
  configuration.workflow("codex-review", [
    "pull_request", "pull_request_review", "pull_request_review_comment",
    "pull_request_review_thread", "issue_comment", "check_run",
  ]);
}

export function run(): void {
  program.run();
  const details = event.details();
  const name = details.get("event").string();
  if (name == "pull_request" || name == "pull_request_review"
    || name == "pull_request_review_comment" || name == "pull_request_review_thread"
    || name == "issue_comment" || name == "check_run") {
    if (details.get("pullNumber").isNull() || details.get("state").string() != "open") return;
    // The host binds trusted evidence to the current head activation. A stale
    // verdict, unresolved thread, or newer unanswered request cannot pass.
    const review = reviews.evaluate();
    checks.report("Codex review", review.state == "success", review.summary);
    return;
  }
  if (name == "runnerless_completion") {
    const result = jobs.lookup<WriteResult>(new OperationRef<WriteResult>("op:publish-ubus-zero"));
    if (result.ok) {
      const value = result.value;
      checks.report("crates-ubus-zero", value.get("status").string() == "succeeded"
        && value.get("value").get("failed").json == "false",
        "crates.io publication: " + value.get("status").string() + " " + value.get("value").get("error").string());
    }
    return;
  }
  // A completed main build can recover a delayed push delivery. Reuse the
  // recipe's operation refs so its completion handler reports the outcome.
  if (name == "workflow_run" && details.get("action").string() == "completed"
    && details.get("workflowPath").string() == ".github/workflows/ci.yml"
    && details.get("conclusion").string() == "success"
    && details.get("pullNumber").isNull()
    && details.get("headRef").string() != ""
    && details.get("headRef").string() == details.get("baseRef").string()) {
    const finalize = releases.finalize(new OperationRequest("release-finalize"));
    releases.propose(new OperationRequest("release-propose").after(finalize));
    return;
  }
  if (name != "workflow_run"
    || details.get("action").string() != "completed"
    || details.get("workflowPath").string() != ".github/workflows/publish-release.yml") return;
  if (details.get("conclusion").string() != "success") {
    checks.report("Crate publishing trigger", false, "The release-asset workflow did not succeed.");
    return;
  }
  const tag = details.get("headRef").string();
  if (!tag.startsWith("ubus-zero-v")) {
    checks.report("Crate publishing trigger", false, "The release-asset workflow did not run on a ubus-zero release tag.");
    return;
  }
  // Runnerless pins "head" to workflow_run.head_sha, not the current main tip.
  const version = repo.readText("version.txt", "head").trim();
  if (tag != "ubus-zero-v" + version) {
    checks.report("Crate publishing trigger", false, "The workflow tag does not match version.txt at its pinned revision.");
    return;
  }
  if (!packages.releasePublished(tag, version)) {
    checks.report("Crate publishing trigger", false, "No stable published release matches the workflow tag and commit.");
    return;
  }
  const request = new RegistryPublishRequest(tag, version)
    .add("ubus-zero", "ubus-zero-" + version + ".cargo-upload");
  // Keep retries of the initial library-only release valid. Package selection
  // comes from the completed workflow's immutable revision.
  if (repo.readText("crates/ubusd/Cargo.toml", "head").includes('name = "ubusd"')) {
    request.add("ubusd", "ubusd-" + version + ".cargo-upload")
      .add("ubus-client", "ubus-client-" + version + ".cargo-upload");
  }
  packages.crates(new OperationRequest("publish-ubus-zero"), request);
  checks.report("Crate publishing trigger", true,
    "Publication requested for " + tag + ". See crates-ubus-zero for the registry outcome.");
}
