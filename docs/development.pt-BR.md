# Desenvolvimento

Como compilar o Asagiri a partir do código-fonte, rodá-lo e como as peças se encaixam.

## Compilação

Requer Rust e, para o kernel e o template de disco, um Docker engine. O Asagiri consegue
compilar o próprio kernel: `DOCKER_CONTEXT=asagiri cargo xtask kernel`.

```sh
cargo xtask kernel    # guest/out/Image
cargo xtask disk      # guest/out/data.img (template ext4 esparso de 64 GiB)
cargo xtask guest     # guest/out/initramfs.cpio
cargo xtask host      # target/release/asagiri, assinado com o entitlement de virtualização
```

Para Macs Intel, adicione `--arch x86_64` a `kernel`, `guest`, `host`, `app`,
`dmg` e `release`: um guest x86-64 em `guest/out/x86_64` (um `bzImage`,
compilado de forma cruzada no Apple Silicon), o host para `x86_64-apple-darwin`
(`rustup target add x86_64-apple-darwin x86_64-unknown-linux-musl`) e
`target/intel/Asagiri.app` com builds Intel do docker CLI, Compose e
buildx, publicado como `Asagiri-<versão>-x86_64.dmg`/`.tar.gz` com o próprio feed,
`latest-mac-x86_64.json`. Cada Mac recebe o próprio app em vez de um app
universal, que carregaria os dois builds das ferramentas docker. No Intel não há
Rosetta para Linux: imagens amd64 rodam nativamente, e imagens sem build
amd64 são recusadas.

No Windows, `cargo xtask windows-dev` compila o engine Linux, o Asagiri Linux e as
ferramentas Docker em `target/asagiri-windows-x86_64` com um `asagiri-win.exe` de debug.
`cargo run -p asagiri-win` usa esse pacote automaticamente. Use `cargo xtask
windows` para o pacote de release e `cargo xtask msi` para o instalador.
Adicione `--arch arm64` a qualquer um dos três para Windows on Arm
(`target/asagiri-windows-aarch64`, `Asagiri-<versão>-arm64.msi`); como testar isso em
uma VM num Mac está em [testing-windows-arm.md](testing-windows-arm.md) (em inglês).

## Uso

```sh
./target/release/asagiri daemon &      # serve ~/.asagiri/run/docker.sock; a VM inicia no primeiro uso
docker context create asagiri --docker host=unix://$HOME/.asagiri/run/docker.sock
docker --context asagiri run --rm alpine echo hello
docker buildx create --name asagiri-build --driver docker-container \
  --driver-opt default-load=true --use asagiri    # para docker build
./target/release/asagiri bench         # Asagiri vs OrbStack vs Docker Desktop
SOAK=600 tests/e2e.sh               # todas as verificações, depois 600 execuções procurando vazamentos
```

Os comandos do Docker também funcionam como comandos `asagiri`, sempre no Asagiri, seja qual
for o contexto docker selecionado: `asagiri run`, `asagiri ps`, `asagiri exec`, `asagiri logs`,
`asagiri build`, `asagiri compose up` e os demais executam o docker CLI contra o
socket do Asagiri. (`asagiri import` é do próprio Asagiri: traz imagens, volumes e
containers do OrbStack ou do Docker Desktop.) As ferramentas sem daemon,
que iniciam o próprio guest, são `asagiri raw run|pull|images|rmi`.

O `asagiri` guarda os dados em `~/Library/Application Support/Asagiri` (altere com
`ASAGIRI_DATA_DIR`). Ele não mexe em `/usr/local/bin/docker`.
O `/var/run/docker.sock` só muda quando você pede: `asagiri setup --docker-socket`
(ou o pedido de senha para os nomes `*.asagiri.local`) o liga ao socket do Asagiri
quando nenhum outro engine o usa, depois que o macOS pede uma senha de administrador,
e instala `/Library/LaunchDaemons/dev.asagiri.docker-socket.plist` para refazer a
ligação após reiniciar. Um socket de outro engine nunca é substituído.

## Arquitetura

```
docker CLI ─┐
compose ────┼─ ~/.asagiri/run/docker.sock ─► asagiri daemon ── vsock ──► guest agent (PID 1)
buildx ─────┤     Docker Engine API          owns the VM            clone3 → container
asagiri UI ────┘
```

- **VM**: o Virtualization.framework da Apple, controlado a partir do Rust via objc2.
  Um kernel próprio de 10 MB (`guest/kernel/`) compilado a partir do allnoconfig, com um
  patch que reduz as esperas de reset do virtio-pci de um tick do scheduler para
  microssegundos, um que entrega TCP grande ao host como GSO, um que mantém
  os arquivos FUSE recém-criados legíveis pelo dono, e uma correção de GRO do upstream que
  mantém os pacotes encaminhados segmentáveis. O Rosetta é compartilhado via virtiofs e registrado no
  binfmt_misc.
- **Agente do guest**: um binário Rust estático de ~900 KB que é o PID 1. Um único loop
  `poll()` atende o host, recolhe processos filhos, encaminha a saída dos containers, repassa
  portas, responde DNS e roda DHCP. `docker cp` e similares rodam em processos
  filhos (fork), para que uma cópia grande nunca o trave.
- **Containers**: um único `clone3` direto em novos namespaces e no
  cgroup do container (`CLONE_INTO_CGROUP`), overlayfs sobre camadas descompactadas
  em um disco de dados ext4, `pivot_root`, capabilities e um filtro seccomp.
  A rede é configurada com mensagens rtnetlink e nf_tables escritas à mão.
  Sem runc, sem shim, sem dockerd.
- **Imagens**: baixadas e enviadas no Mac (`crates/asagiri-image`), transmitidas
  para o guest uma única vez e descompactadas lá.

## Organização

| Caminho | O quê |
|---------|-------|
| `crates/asagiri` | o binário `asagiri`: daemon, Docker API, CLI, UI, bench |
| `crates/asagiri-vmm` | VM do Virtualization.framework |
| `crates/asagiri-init` | PID 1 do guest, agente e runtime de containers |
| `crates/asagiri-proto` | protocolo de comunicação host ↔ guest (no_std, sem alocação por mensagem) |
| `crates/asagiri-image` | referências de imagem, cliente de registry, blob store, arquivos |
| `guest/kernel` | configuração e patches do kernel |
| `tests/e2e.sh` | ~350 verificações pelo `docker` CLI real, mais um soak de vazamentos (`K8S=1` adiciona Kubernetes) |
| `tests/linux-host.sh` | o build para host Linux, executado em um container privilegiado |
| `tests/finder-mount.sh` | o mount do Finder no Mac em `~/Asagiri`: cobre só uma pasta vazia ou o próprio mount antigo, nunca seus arquivos nem outro mount |
| `contrib/` | unit do systemd, scripts de instalação para WSL2 e Windows |
| `installer/windows` | o MSI do Windows (WiX 5): `asagiri.wxs`, licença, bitmaps dos diálogos |
| `packaging/` | `.deb` e `.rpm` para Linux (`asagiri` com o docker CLI, `asagiri-lite` só o engine), o plano para Homebrew |
| `xtask` | tarefas de build |
| `bench/results.json` | resultados medidos; o site mantém uma cópia |
| `brand/` | marca (o site e a saída bruta dos benchmarks ficam fora deste repositório) |
