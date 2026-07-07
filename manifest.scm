(use-modules (guix packages)
             (guix search-paths)
             (gnu packages rust)
             (gnu packages cmake)
             (gnu packages commencement))

(define gcc-toolchain-with-cc
  (package
    (inherit gcc-toolchain)
    (native-search-paths
     (cons (search-path-specification
            (variable "CC")
            (files '("bin/gcc"))
            (file-type 'regular)
            (separator #f))
           (package-native-search-paths gcc-toolchain)))))

(packages->manifest
 (list rust
       (list rust "cargo")
       cmake
       gcc-toolchain-with-cc))
