import {
  workflow, configuration, event, repo, packages, checks, jobs,
  OperationRef, OperationRequest, RegistryPublishRequest, WriteResult,
} from "runnerless/v1";

const program = workflow().releasePlease();

export function configure(): void {
  program.configure();
  configuration.workflow("crate-publishing", ["workflow_run", "runnerless_completion"]);
}

export function run(): void {
  program.run();
  const details = event.details();
  if (details.get("event").string() == "runnerless_completion") {
    const result = jobs.lookup<WriteResult>(new OperationRef<WriteResult>("op:publish-ubus-zero"));
    if (result.ok) {
      const value = result.value;
      checks.report("crates-ubus-zero", value.get("status").string() == "succeeded"
        && value.get("value").get("failed").json == "false",
        "crates.io publication: " + value.get("status").string() + " " + value.get("value").get("error").string());
    }
    return;
  }
  if (details.get("event").string() != "workflow_run"
    || details.get("action").string() != "completed"
    || details.get("workflowPath").string() != ".github/workflows/publish-release.yml"
    || details.get("conclusion").string() != "success") return;
  const tag = details.get("headRef").string();
  if (!tag.startsWith("ubus-zero-v")) return;
  // Runnerless pins "head" to workflow_run.head_sha, not the current main tip.
  const version = repo.readText("version.txt", "head").trim();
  if (tag != "ubus-zero-v" + version) return;
  if (!packages.releasePublished(tag, version)) return;
  const request = new RegistryPublishRequest(tag, version)
    .add("ubus-zero", "ubus-zero-" + version + ".cargo-upload");
  // Keep retries of the initial library-only release valid. Package selection
  // comes from the completed workflow's immutable revision.
  if (repo.readText("crates/ubusd/Cargo.toml", "head").includes('name = "ubusd"')) {
    request.add("ubusd", "ubusd-" + version + ".cargo-upload")
      .add("ubus-client", "ubus-client-" + version + ".cargo-upload");
  }
  packages.crates(new OperationRequest("publish-ubus-zero"), request);
}
