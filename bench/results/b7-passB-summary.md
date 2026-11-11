| part | metric | off | on | on/off per round |
|---|---|---|---|---|
| cold | first byte p50 us | 3726 [3660-4347] | 3619 [3444-4421] | 0.941 [0.8118-1.187] (errors 0) |
| cold | first byte p99 us | 8272 [7634-1.32e+04] | 6767 [5974-9900] | 0.8181 [0.4526-1.201] (errors 0) |
| mixed-c1 | first byte p50 us | 156 [139-159] | 163 [152-167] | 1.044 [1.013-1.099] (errors 0) |
| mixed-c1 | first byte p99 us | 1723 [1532-1841] | 1794 [1547-2002] | 1.062 [0.9756-1.087] (errors 0) |
| mixed-c16 | first byte p50 us | 3965 [3770-4031] | 3879 [3735-5263] | 0.9783 [0.9328-1.396] (errors 0) |
| mixed-c16 | first byte p99 us | 8493 [8218-8970] | 8800 [7786-1.204e+04] | 1.036 [0.9403-1.465] (errors 0) |
| cold-existing | first byte p50 us | 847 [789-931] | 868 [844-894] | 1.055 [0.9066-1.109] (errors 0) |
| cold-existing | first byte p99 us | 3505 [1673-6141] | 2924 [1968-5347] | 0.9582 [0.4761-2.89] (errors 0) |
| mixed-c1 | req/s | 3291 [3157-3744] | 3172 [3024-3487] | 0.9406 [0.9313-1.01] |
| mixed-c16 | req/s | 3901 [3834-4090] | 3879 [2882-4138] | 1.011 [0.7046-1.061] |
off PSS MiB after mixed: 48.14 [48-48.63] log bytes: 318 [318-318]
on PSS MiB after mixed: 48.06 [47.78-48.82] log bytes: 7.082e+06 [7.082e+06-7.083e+06]
