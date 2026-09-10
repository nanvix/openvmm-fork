// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build the source-owned x86_64 Xen PVH test guest.

use crate::common::CommonProfile;
use flowey::node::prelude::*;

#[derive(Serialize, Deserialize)]
pub struct GuestTestPvhOutput {
    #[serde(rename = "guest_test_pvh")]
    pub bin: PathBuf,
}

impl Artifact for GuestTestPvhOutput {}

flowey_request! {
    pub struct Request {
        pub profile: CommonProfile,
        pub guest_test_pvh: WriteVar<GuestTestPvhOutput>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::run_cargo_build::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut outputs_by_profile = std::collections::BTreeMap::<_, Vec<_>>::new();
        for Request {
            profile,
            guest_test_pvh,
        } in requests
        {
            outputs_by_profile
                .entry(profile)
                .or_default()
                .push(guest_test_pvh);
        }

        for (profile, outputs) in outputs_by_profile {
            let output = ctx.reqv(|v| crate::run_cargo_build::Request {
                crate_name: "guest_test_pvh".into(),
                out_name: "guest_test_pvh".into(),
                crate_type: flowey_lib_common::run_cargo_build::CargoCrateType::Bin,
                profile: profile.into(),
                features: Default::default(),
                target: target_lexicon::Triple {
                    architecture: target_lexicon::Architecture::X86_64,
                    operating_system: target_lexicon::OperatingSystem::None_,
                    environment: target_lexicon::Environment::Unknown,
                    vendor: target_lexicon::Vendor::Custom(target_lexicon::CustomVendor::Static(
                        "minimal_rt",
                    )),
                    binary_format: target_lexicon::BinaryFormat::Unknown,
                },
                no_split_dbg_info: true,
                extra_env: None,
                pre_build_deps: Vec::new(),
                output: v,
            });

            ctx.emit_minor_rust_step("report built PVH test guest", |ctx| {
                let output = output.claim(ctx);
                let outputs = outputs.claim(ctx);
                move |rt| {
                    let bin = match rt.read(output) {
                        crate::run_cargo_build::CargoBuildOutput::ElfBin { bin, dbg: None } => bin,
                        _ => panic!("PVH test guest build produced an unexpected output type"),
                    };
                    let output = GuestTestPvhOutput { bin };
                    for var in outputs {
                        rt.write(var, &output);
                    }
                }
            });
        }

        Ok(())
    }
}
