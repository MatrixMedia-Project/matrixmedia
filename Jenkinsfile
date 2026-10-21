// MatrixMedia backend lane (MatrixMedia-Project/matrixmedia).
//
//   BRANCH        — git branch or tag checked out for this run.
//   DEPLOY_TARGET — none: build + test only. dev: also run mm-deliver-dev
//                   (dev server + restart of mm-core + health).
// Recommended agent labels (when dedicated agents exist): rust | linux.
// Needs a Rust toolchain (rustup, stable per rust-toolchain.toml); the Doctor
// stage fails fast with a TOOLCHAIN GAP when cargo is missing.

pipeline {
    agent any

    parameters {
        string(name: 'BRANCH', defaultValue: 'development',
            description: 'Git branch or tag to build.')
        choice(name: 'DEPLOY_TARGET', choices: ['none', 'dev'],
            description: 'none: build + test only. dev: also deliver to the dev server via mm-deliver-dev.')
    }

    options {
        timestamps()
        buildDiscarder(logRotator(numToKeepStr: '20'))
        timeout(time: 90, unit: 'MINUTES')
    }

    stages {
        stage('Doctor') {
            steps {
                sh '''
                    if ! command -v cargo >/dev/null 2>&1; then
                        echo "TOOLCHAIN GAP: cargo is not installed on this agent."
                        echo "Install the Rust toolchain (see rust-toolchain.toml), or pin this job to a rust-labelled agent."
                        exit 1
                    fi
                    cargo --version && rustc --version
                '''
            }
        }

        stage('Build') {
            steps {
                sh 'cargo build --workspace --locked'
            }
        }

        stage('Test') {
            steps {
                sh 'cargo test --workspace --locked'
            }
        }

        stage('Provenance') {
            steps {
                sh '''
                    git rev-parse HEAD > .git-commit 2>/dev/null || echo unknown > .git-commit
                    { echo "job=$JOB_NAME"; echo "build=$BUILD_NUMBER"; echo "branch=${BRANCH}"; echo -n "commit="; cat .git-commit; } > build-info.txt
                    cat build-info.txt
                '''
            }
        }

        stage('Deliver to dev') {
            when { expression { return params.DEPLOY_TARGET == 'dev' } }
            steps {
                build job: 'mm-deliver-dev', wait: true, propagate: true, parameters: [
                    string(name: 'SOURCE_JOB', value: env.JOB_NAME),
                    string(name: 'SOURCE_BUILD', value: env.BUILD_NUMBER),
                    string(name: 'SERVICE', value: 'mm-core'),
                ]
            }
        }
    }

    post {
        always {
            archiveArtifacts artifacts: 'build-info.txt', allowEmptyArchive: false
        }
    }
}
