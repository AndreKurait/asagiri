# Published-port TCP relay on macOS

I compared the published-port relay with and without its two `TcpStream::try_clone()` calls. Both host executables came from the same checkout and release-profile toolchain. Four alternating rounds of latency and bulk runs used fresh private app bundles, stores, sockets, homes and two-vCPU, 2 GiB guests on one Apple M3 Pro. The guest image and Docker client came from the same published `v0.1.16-dev.349` archive in every run.

| Run | Daemon TCP FDs for 32 flows | Persistent / new echo, ms | Download / upload, MiB/s | Daemon CPU ns/byte, down / up | Idle CPU s, latency / bulk | Idle RSS KiB, latency / bulk |
|---|---:|---:|---:|---:|---:|---:|
| 1-base | 128 | 0.260 / 0.858 | 1,743 / 1,014 | 0.373 / 0.652 | 0.00 / 0.00 | 25,120 / 23,280 |
| 2-candidate | 64 | 0.268 / 0.890 | 1,758 / 1,008 | 0.373 / 0.652 | 0.00 / 0.00 | 25,216 / 22,656 |
| 3-candidate | 64 | 0.256 / 0.879 | 1,749 / 999 | 0.373 / 0.671 | 0.00 / 0.00 | 25,104 / 22,576 |
| 4-base | 128 | 0.250 / 0.890 | 1,749 / 979 | 0.391 / 0.671 | 0.00 / 0.00 | 25,280 / 22,656 |

`lsof -nP -a -p PID -iTCP -sTCP:ESTABLISHED -Ffn` associated each descriptor with its endpoint tuple. For 32 simultaneous echoed client connections, the baseline had 64 client-side and 64 guest-side descriptors. The candidate had 32 of each. Every connection returned `END` and EOF after a client half-close. All fixture endpoint pairs disappeared from the daemon’s established-socket list after closure. The table counts the relay’s descriptors only.

Each timing row gives within-run medians for 80 persistent and 24 fresh 32-byte echoes and four validated 256 MiB transfers per direction. All 416 exchanges and 32 transfers used the observed guest TCP route. Each latency run also verified an echo after a five-second idle interval and checked every byte of an 8 MiB exchange with a paused reader. Idle CPU is process time over five seconds, sampled to 0.01 s. The eight owned containers were removed, all eight daemons exited zero and all temporary roots were removed. Two runs per variant on one machine establish the descriptor reduction. Timing and CPU figures are descriptive.

Host base `03ede627534befd06ab756bd909a0e0acde0fd10`, candidate patch SHA-256 `e0225083a438f326d533f96b82d131ef3ca8a41ea154afcfadda47672aa778b3`, toolchain `rustc 1.99.1 (58cf8f9c3 2026-10-13)`. The executable SHA-256 values before ad hoc fixture signing were `9459759b3937abf1381ec1d623668683e89d0813f17df3a9373064e383b92d68` and `3e82ddb38630e9447ff82ef60cac1b9bd449ad25fe305ab177191c24f822d6cb`. Both used `cargo build --locked --release -p asagiri --bin asagiri`. The guest archive SHA-256 was `3d8b05725e0c22558b3528524bd2e8975ee668c6a67717702c08d613d1274426` and its kernel reported `6.18.58`. The Node image ID was `sha256:b06790a7ce8f827fd695d4a148ba260b351d658af126b6deb27af3823a1675a1`.

<details><summary>Raw observations in trial order</summary>

Echo rows give wall nanoseconds. Transfer rows give direction, wall nanoseconds and daemon CPU ns/byte. FD rows give accepted and guest descriptor counts, remaining fixture connections and whether the sender was still pending when reads resumed.

```csv
1-base,persistent,549542,347167,323625,314333,268083,271917,262750,262417,296250,319542,283209,247500,259791,297584,295000,297375,299917,276541,285709,284042,304000,322834,261792,259084,260208,226166,300458,268167,278000,249042,288333,235250,228208,238458,261958,251292,259792,272125,246417,260375
1-base,new,914375,869541,885750,993458,1099750,1018375,1077458,807750,876750,855542,769708,822083
1-base,new,837834,807209,835291,857208,858916,813292,818042,812875,954084,911875,855042,866375
1-base,persistent,781000,556917,585167,513916,533917,510125,531500,517208,521250,348875,252625,253291,260042,247125,223333,225000,220000,237292,222666,224416,211292,223417,210041,220958,218625,236125,237500,224959,217291,213167,221292,239750,235875,206584,209166,219083,237458,220542,216542,220958
1-base,download,160709292,0.372529
1-base,upload,247500375,0.670552
1-base,upload,252630667,0.633299
1-base,download,147397584,0.409782
1-base,upload,252344333,0.633299
1-base,download,146301959,0.372529
1-base,download,143710000,0.372529
1-base,upload,263397042,0.670552
1-base,fd,64,64,0,False
2-candidate,persistent,629458,301916,289584,285333,248000,280042,286584,247042,264667,280292,281917,261750,261125,266583,295959,271541,261584,268708,246375,282458,264041,252625,236333,245459,258250,235667,263583,231542,253208,239458,269666,266583,260792,281291,252958,244167,237625,244625,223166,224458
2-candidate,new,889125,858416,922833,985500,1374708,1087083,955292,882250,921875,888250,877416,828833
2-candidate,new,818209,858167,892334,939750,890166,848583,907375,898500,887417,876542,918792,868417
2-candidate,persistent,728333,412167,353709,262333,267292,333208,392584,361750,382291,388167,397791,394875,366959,310875,311750,281042,282292,274584,273500,275042,274250,270375,241750,266042,292875,298250,292667,295583,252542,218083,209833,266416,325708,343208,237250,237166,252834,231750,213250,253250
2-candidate,download,160588292,0.372529
2-candidate,upload,255556042,0.633299
2-candidate,upload,252463208,0.633299
2-candidate,download,141626875,0.372529
2-candidate,upload,252150708,0.670552
2-candidate,download,137352542,0.372529
2-candidate,download,149894666,0.372529
2-candidate,upload,258270667,0.670552
2-candidate,fd,32,32,0,False
3-candidate,persistent,488834,257541,266458,277416,288833,265708,271458,277167,260208,257791,282334,244834,237667,245834,271042,285125,259000,241209,243250,251042,236458,241666,241709,217208,239708,227125,217500,231167,220000,211625,220666,237583,244625,239250,241625,251916,255125,247417,244708,257500
3-candidate,new,854750,923333,906042,878958,1219417,2046542,978125,1103916,934042,878208,865167,856083
3-candidate,new,963000,829000,833875,795583,762083,871958,832917,1021334,864542,933625,801125,895959
3-candidate,persistent,602792,327500,290916,304417,376584,286292,312333,339750,310333,334041,318375,289125,330291,335333,312083,319125,301542,315500,284959,297959,280833,266875,271167,262208,256917,244500,240000,232917,237333,214291,215750,228834,242459,254125,232000,243000,240292,242458,224083,200708
3-candidate,download,163317250,0.372529
3-candidate,upload,251942083,0.670552
3-candidate,upload,251062833,0.633299
3-candidate,download,145567042,0.409782
3-candidate,upload,260481500,0.670552
3-candidate,download,147252917,0.372529
3-candidate,download,141761500,0.372529
3-candidate,upload,261576666,0.670552
3-candidate,fd,32,32,0,False
4-base,persistent,524583,282083,275917,250792,292292,248417,255583,244458,255209,248167,384833,359083,267375,275417,271750,281208,251833,246917,223209,215667,233875,223125,235250,245250,231709,229250,229458,230125,248916,233333,217333,209875,218792,244584,330084,323917,286333,214709,223792,250583
4-base,new,816625,861708,786333,892042,1092291,1086333,1427667,840458,1804333,1645792,957458,880875
4-base,new,914792,925541,883709,932000,788291,796500,1429625,887750,802083,805708,819208,1241458
4-base,persistent,557833,245417,234125,233750,236583,241041,221208,216708,234500,238917,217750,227084,236000,211875,226708,205584,260125,295375,319750,339000,353416,340125,332375,347000,270708,214125,223125,218500,393291,337750,365000,337292,330750,315791,321458,324333,334708,325542,361583,376042
4-base,download,165782208,0.372529
4-base,upload,261698833,0.670552
4-base,upload,263511583,0.670552
4-base,download,147066084,0.409782
4-base,upload,261191375,0.670552
4-base,download,145750250,0.409782
4-base,download,145387542,0.372529
4-base,upload,249537083,0.633299
4-base,fd,64,64,0,False
```

</details>

The measured production change is `2257299a7e48c3cf3e13e861a44ab265e6edb184`.

## Guest-loopback vsock and TCP fallback

On macOS, `docker run --network host -p 127.0.0.1::19457` publishes a port on the Mac while the container uses the guest network namespace. I bound both test services exclusively to loopback and checked the live container’s network mode. Each Mac client got the expected payload, end marker and EOF. `lsof` showed the accepted Mac connection and no TCP connection from the daemon to the guest. This exercises the production vsock relay independently of a VPN or a host network change.

| Build | Persistent / new echo, ms | Download / upload, MiB/s | Host CPU ns/byte, down / up |
|---|---:|---:|---:|
| base | 0.266 / 0.780 | 410 / 634 | 2.235 / 0.577 |
| candidate | 0.208 / 0.804 | 413 / 677 | 2.235 / 0.522 |

One preliminary bridge-mode baseline guest also fell back naturally when its NAT connection timed out. All eight validated transfers completed over vsock, with medians of 402 MiB/s down and 559 MiB/s up. The matched TCP series above starts timing after a probe establishes the NAT route and checks the route on the timed connections. A specific VPN configuration remains untested.

<details><summary>Guest-loopback vsock raw observations</summary>

Echo rows give wall nanoseconds. Bulk rows give direction, wall nanoseconds and process CPU ns/byte.

```csv
base,persistent,513375,262250,270125,268458,271458,257209,230042,240334,244125,229042,216667,213459,209042,229666,223750,199375,183708,198500,201041,195500,194792,224125,266708,273667,265292,278625,273750,260750,265708,262042,259666,260041,263333,256625,230042,194000,194167,193292,192250,186083
base,new,715041,678375,773208,808917,1026833,978375,1177167,1091875,1253792,1278500,786542,675125
base,new,695000,694583,695917,734167,743042,882791,944291,842667,686125,683458,793042,726291
base,persistent,379875,319792,336334,271125,235292,229875,244709,242042,235042,240208,222500,258417,310417,313542,288208,304583,338750,348041,340584,338625,336625,360166,350208,341084,318833,281250,277875,290167,300250,299250,291625,302917,290084,282250,290709,291625,290083,294000,273875,307792
base,download,647700250,2.346933
base,upload,391530375,0.558794
base,upload,416670000,0.596046
base,download,635390916,2.235174
base,upload,382144250,0.521541
base,download,601385167,2.160668
base,download,614845708,2.235174
base,upload,422116583,0.633299
candidate,persistent,563458,230708,225625,231583,231167,220250,258250,227333,253291,234084,230333,207875,229584,239416,204250,203625,202250,192417,192292,191458,186709,205917,180167,216042,196666,194500,205000,186000,196375,184625,199125,196959,210792,196667,199834,194875,203041,201875,210584,214000
candidate,new,926625,835792,815500,812458,989250,981417,1212833,836500,701041,715416,762708,757167
candidate,new,743375,869209,924083,831125,737042,709417,739834,980250,749792,796208,757750,749667
candidate,persistent,459333,226250,246416,239416,244416,237750,219750,211542,230167,217166,224708,228584,205458,214250,227834,205209,226834,218458,190875,212333,206375,208875,212583,178666,189917,193667,188250,205625,221209,208250,203375,225667,207666,190583,203208,206209,204167,183250,203375,189000
candidate,download,619607583,2.235174
candidate,upload,383922083,0.521541
candidate,upload,383826542,0.521541
candidate,download,618642083,2.235174
candidate,upload,297786042,0.447035
candidate,download,638612708,2.346933
candidate,download,586842250,2.160668
candidate,upload,373122458,0.521541
```

</details>

## Flow control

On a separate TCP connection for each build, the client paused reads for 250 ms during a 32 MiB echoed write with a 32 KiB client send buffer. Both senders were still pending at that point. Resuming reads delivered every byte in order, then the application end marker and EOF. Both daemons exited and their fixture roots were removed.

## Restart, DNS and descriptor regression

Both builds served a fresh client connection through the guest TCP uplink after `docker restart`. Before and after the restart, Node’s resolver found the Docker network alias `echo-peer` at the container’s inspected address, and its system lookup found the `--add-host` value for `test-alias.invalid`. The private containers and networks were removed.

On macOS the focused regression opens one live relay, exchanges a byte and counts only the descriptors with the two fixture endpoint tuples. Applied to the base, it observed two descriptors for the accepted socket and expected one. All 19 port tests passed against the candidate, including the new regression, idle/half-close and reset while the opposite peer is quiet.

```sh
cargo test --locked -p asagiri --bin asagiri daemon::ports::
```

The checks covered this Apple Silicon machine running macOS 26.7.3. The comparison and earlier unchanged-release controls are tracked in [issue #275](https://github.com/AndreKurait/asagiri/issues/275).
