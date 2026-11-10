# Windows x64 release

The public installer is `VoiceIsolator-Setup-x64.exe`. It installs for the current Windows user and requires Windows 10 22H2 or Windows 11 x64. The package bundles the CPU provider, the optimized sherpa build, and the pinned VoxLock INT8 pair. The optional CUDA task downloads the CUDA runtime, cuBLAS and cuDNN directly from NVIDIA after license acceptance. It also downloads Microsoft's CUDA-enabled ONNX Runtime and the VoxLock FP32 chunk model. Setup verifies all five SHA-256 pins in `cmake/Dependencies.cmake` before copying them. Users install the audio driver separately through the vendor link shown in the app.

## Build and sign

1. Review the first commit before creating the public repository. Check that the VoxLock Git submodule, sherpa revision and patch, and model hashes match the reviewed source. Keep enrollment, recordings, test artifacts, private signing keys, and local build outputs out of Git.
2. Generate the publisher key with `minisign -G -p voiceisolator.pub -s voiceisolator.key`. Store the password-protected secret and its backup outside the repository. Keep the same key for successive releases so installed versions can verify updates. The second line of the `.pub` file is the public key supplied at build time.
3. Tag the reviewed commit with `v` followed by the exact `Cargo.toml` version. From PowerShell 7, prepare the tested CPU SDK and pinned files.

   ```powershell
   git submodule update --init --recursive
   cmake -DVOICEISOLATOR_TEST_FIXTURES=ON -P scripts/fetch-deps.cmake
   .\scripts\build-pcm-sdk.ps1 -Optimized
   $publicKey = (Get-Content $publisherPublicKey | Select-Object -Last 1).Trim()
   .\scripts\build-installer.ps1 -Iscc $iscc -VCRuntimeDir $x64CrtDir -UpdatePublicKey $publicKey
   ```

   `$iscc` points to Inno Setup 6.7.4's `ISCC.exe`. `$x64CrtDir` is the official `VC\Redist\MSVC\<version>\x64\Microsoft.VC*.CRT` directory from a licensed Visual Studio installation. For a Visual Studio 2022 SDK build, pass `-Generator 'Visual Studio 17 2022'` to `build-pcm-sdk.ps1` and `-NativeBuild sdk-vs2022` to `build-installer.ps1`. The build gathers the installed license texts for native and Rust dependencies, UI fonts, the VoxLock weight attribution, and the corresponding Eigen source link. Setup asks users to accept Microsoft's terms for the three bundled runtime DLLs. The build fails when a required text or input is missing.

4. The optional **Build reviewed Windows release** workflow builds and tests the exact tag and uploads a review artifact. Set the repository variable `MINISIGN_PUBLIC_KEY` to the public key line. Review the artifact and confirm the Visual Studio redistribution terms for the build source. The workflow has read-only repository permissions and uses no signing secret.
5. Keep the reviewed installer and its `.installer-build.json` together in `out/windows-x64-pcm/Release` and sign locally.

   ```powershell
   .\scripts\sign-release.ps1 -Minisign $minisign -SecretKey $publisherSecretKey -PublicKeyFile $publisherPublicKey
   ```

   The script checks the installer version, the build hash and public key, then signs the manifest and verifies the signature. Use the installer, `VoiceIsolator-release.json`, and `VoiceIsolator-release.json.minisig` as the three release assets. Keep the build record internal. Create a draft from the reviewed tag and publish only after the checks below. Enable immutable releases before publishing. The installer is not Authenticode-signed, so Windows may show a reputation warning. [GitHub immutable releases](https://docs.github.com/en/code-security/concepts/supply-chain-security/immutable-releases)

## Verify before publishing

- Install on clean Windows 10 22H2 and Windows 11 x64 accounts with a standard user. Confirm the Start menu shortcut and uninstall entry. Inspect the installed `licenses` folder and confirm the six expected native DLLs. Verify the CPU route works offline after separately installing VB-CABLE.
- Check **Start with Windows** on and off. Sign out and back in after isolating and after explicitly stopping. Verify startup in the tray, that **X** keeps audio running, and that **Quit** closes the engine. Unplug the chosen cable or mic and confirm auto-resume stays silent and asks for a manual start after the wait. Try an invalid voice profile. Opening the shortcut again should reveal the existing window.
- Exercise a signed upgrade with both builds using the same test key. Start the update while isolating and check that audio continues during the download. Check that the audio engine stops when the verified installer opens, the new app reopens in standby, and the profile and settings survive. Verify that recording blocks installation. Cancel during download and in setup, interrupt the network, alter the manifest and installer separately, and try a mismatched version tag. Verify same-version repair and downgrade rejection. Use a test installation and test key for these checks.
- On a compatible NVIDIA PC, select CUDA support in Setup opened by an in-app update. Verify that declining the terms or cancelling the downloads leaves the existing installation intact. Install and switch between CPU and CUDA while isolating. Verify that an offline repair reuses the same complete runtime, that deselecting the task removes it, and that a CPU-only install works offline.
- Confirm uninstall removes the app, shortcut and startup registration. Voice data and settings under `%LOCALAPPDATA%\VoiceIsolator` remain available for reinstall. After publishing, test the download signed out and verify the intended speaker reaches Discord on the target PC.
